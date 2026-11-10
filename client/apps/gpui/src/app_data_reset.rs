use std::path::PathBuf;

/// Every filesystem root owned by SQLLY. Removing these roots leaves the next
/// launch with the same on-disk state as a fresh install.
pub fn persisted_data_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        dirs::data_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("SQLLY"),
    ];

    if let Some(config_dir) = dirs::config_dir() {
        paths.push(config_dir.join("SQLLY"));
    } else {
        paths.extend([
            PathBuf::from("connections.json"),
            PathBuf::from("entra-accounts.json"),
            PathBuf::from("intellisense-rules.jsonc"),
            PathBuf::from("query-ai-summary.json"),
        ]);
    }

    // Never recursively delete the configured catalog root itself:
    // SQLLY_CATALOG_DIR may be a shared parent selected by the user or
    // deployment environment.
    paths.push(sqlly_services::catalog::catalog_root());
    #[cfg(not(target_arch = "wasm32"))]
    paths.push(crate::result_cache::base_dir());
    paths
}

fn reset_marker_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".sqlly-reset-pending")
}

/// Record the reset outside SQLLY's removable roots, then delete every current
/// persisted path. The marker intentionally remains until the next launch so
/// cleanup can remove any state recreated by a late background writer.
pub fn schedule_reset(credential_result: Result<(), String>) -> Result<(), String> {
    schedule_reset_at(
        credential_result,
        &reset_marker_path(),
        &persisted_data_paths(),
    )
}

fn schedule_reset_at(
    credential_result: Result<(), String>,
    marker: &PathBuf,
    paths: &[PathBuf],
) -> Result<(), String> {
    credential_result?;
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(marker, b"reset pending").map_err(|error| error.to_string())?;
    clear_paths(paths)
}

/// Complete a requested reset before application state is loaded. A failure
/// leaves the marker in place so the next launch retries instead of silently
/// starting with a partially reset profile.
pub fn finish_pending_reset() -> Result<(), String> {
    finish_pending_reset_at(&reset_marker_path(), &persisted_data_paths())
}

fn finish_pending_reset_at(marker: &PathBuf, paths: &[PathBuf]) -> Result<(), String> {
    if !marker.exists() {
        return Ok(());
    }
    clear_paths(paths)?;
    std::fs::remove_file(marker).map_err(|error| error.to_string())
}

fn clear_paths(paths: &[PathBuf]) -> Result<(), String> {
    let mut failures = Vec::new();
    let mut removed = Vec::<PathBuf>::new();

    for path in paths {
        if removed.iter().any(|root| path.starts_with(root)) || !path.exists() {
            continue;
        }

        let result = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match result {
            Ok(()) => removed.push(path.clone()),
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_paths_removes_files_directories_and_nested_targets() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data/SQLLY");
        let config = temp.path().join("config/SQLLY");
        let catalog = data.join("Catalogs");
        std::fs::create_dir_all(&catalog).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(data.join("preferences.json"), "prefs").unwrap();
        std::fs::write(catalog.join("catalog.sqlite"), "catalog").unwrap();
        std::fs::write(config.join("connections.json"), "servers").unwrap();

        clear_paths(&[data.clone(), config.clone(), catalog]).unwrap();

        assert!(!data.exists());
        assert!(!config.exists());
    }

    #[test]
    fn clear_paths_ignores_missing_targets() {
        let temp = tempfile::tempdir().unwrap();
        clear_paths(&[temp.path().join("not-created")]).unwrap();
    }

    #[test]
    fn clear_paths_removes_fallback_files() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("connections.json");
        std::fs::write(&file, "servers").unwrap();

        clear_paths(std::slice::from_ref(&file)).unwrap();

        assert!(!file.exists());
    }

    #[test]
    fn configured_paths_cover_data_config_and_catalog() {
        let paths = persisted_data_paths();
        assert!(paths.len() >= 3);
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(std::path::Path::new("SQLLY")))
        );
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(std::path::Path::new("Catalogs")))
        );
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(std::path::Path::new("result-cache")))
        );
    }

    #[test]
    fn credential_failure_does_not_schedule_or_delete_files() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("reset-pending");
        let data = temp.path().join("SQLLY");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("connections.json"), "servers").unwrap();

        let result = schedule_reset_at(
            Err("keychain unavailable".into()),
            &marker,
            std::slice::from_ref(&data),
        );

        assert!(result.is_err());
        assert!(!marker.exists());
        assert!(data.exists());
    }

    #[test]
    fn scheduled_reset_is_repeated_and_marker_removed_on_next_launch() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("reset-pending");
        let data = temp.path().join("SQLLY");
        std::fs::create_dir(&data).unwrap();

        schedule_reset_at(Ok(()), &marker, std::slice::from_ref(&data)).unwrap();
        assert!(marker.exists());
        assert!(!data.exists());

        // Simulate a late background writer recreating app state before quit.
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("workspace-session.json"), "late write").unwrap();
        finish_pending_reset_at(&marker, std::slice::from_ref(&data)).unwrap();

        assert!(!marker.exists());
        assert!(!data.exists());
    }
}
