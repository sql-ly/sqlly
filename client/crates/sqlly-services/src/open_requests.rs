use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    pub id: Uuid,
    pub raw: String,
    pub created_at: String,
}

pub const OPEN_REQUEST_TTL_SECS: u64 = 120;

#[cfg(not(target_arch = "wasm32"))]
pub fn open_requests_dir() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("SQLLY_OPEN_REQUESTS_DIR") {
        return std::path::PathBuf::from(path);
    }
    if let Some(dir) = dirs::data_dir() {
        let dir = dir.join("SQLLY").join("open-requests");
        let _ = ensure_open_requests_dir(&dir);
        return dir;
    }
    std::path::PathBuf::from("open-requests")
}

#[cfg(not(target_arch = "wasm32"))]
fn ensure_open_requests_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(dir) {
            let mode = meta.permissions().mode();
            if mode & 0o777 != 0o700 {
                let _ = std::fs::set_permissions(
                    dir,
                    std::fs::Permissions::from_mode(mode & !0o777 | 0o700),
                );
            }
        }
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn write_request(raw: &str) -> Result<std::path::PathBuf, String> {
    write_request_in(&open_requests_dir(), raw)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn write_request_in(dir: &std::path::Path, raw: &str) -> Result<std::path::PathBuf, String> {
    ensure_open_requests_dir(dir).map_err(|error| error.to_string())?;
    let request = OpenRequest {
        id: Uuid::new_v4(),
        raw: raw.to_string(),
        created_at: Utc::now().to_rfc3339(),
    };
    let path = dir.join(format!("{}.json", request.id));
    let bytes = serde_json::to_vec_pretty(&request).map_err(|error| error.to_string())?;
    write_owner_only_atomic(&path, &bytes).map_err(|error| error.to_string())?;
    Ok(path)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn take_requests(now: DateTime<Utc>) -> Result<Vec<OpenRequest>, String> {
    take_requests_in(&open_requests_dir(), now)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn take_requests_in(
    dir: &std::path::Path,
    now: DateTime<Utc>,
) -> Result<Vec<OpenRequest>, String> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut requests = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| error.to_string())? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let request: OpenRequest = match serde_json::from_str(&text) {
            Ok(request) => request,
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        let _ = std::fs::remove_file(&path);
        let Ok(created) = DateTime::parse_from_rfc3339(&request.created_at) else {
            continue;
        };
        let created = created.with_timezone(&Utc);
        if now.signed_duration_since(created).num_seconds() > OPEN_REQUEST_TTL_SECS as i64 {
            continue;
        }
        requests.push(request);
    }
    requests.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(requests)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn touch_instance_marker() -> std::io::Result<()> {
    touch_instance_marker_in(&open_requests_dir())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn touch_instance_marker_in(dir: &std::path::Path) -> std::io::Result<()> {
    ensure_open_requests_dir(dir)?;
    let path = dir.join("instance.lock");
    std::fs::write(&path, Utc::now().to_rfc3339())?;
    #[cfg(unix)]
    tighten_owner_only_file(&path);
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn instance_is_alive(now: std::time::SystemTime, max_age: std::time::Duration) -> bool {
    instance_is_alive_in(&open_requests_dir(), now, max_age)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn instance_is_alive_in(
    dir: &std::path::Path,
    now: std::time::SystemTime,
    max_age: std::time::Duration,
) -> bool {
    let path = dir.join("instance.lock");
    let Ok(meta) = std::fs::metadata(&path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    now.duration_since(modified)
        .ok()
        .is_some_and(|age| age <= max_age)
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(unix)]
fn tighten_owner_only_file(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o077));
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(unix))]
fn tighten_owner_only_file(_path: &std::path::Path) {}

#[cfg(not(target_arch = "wasm32"))]
fn write_owner_only_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_open_requests_dir(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("request.json");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        tighten_owner_only_file(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration as StdDuration;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn write_then_take_returns_and_removes_in_order() {
        let dir = tempfile::tempdir().unwrap();
        write_request_in(dir.path(), "sqlly://open?connection=a").unwrap();
        thread::sleep(StdDuration::from_millis(5));
        write_request_in(dir.path(), "sqlly://open?connection=b").unwrap();
        let taken = take_requests_in(dir.path(), Utc::now()).unwrap();
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].raw, "sqlly://open?connection=a");
        assert_eq!(taken[1].raw, "sqlly://open?connection=b");
        assert!(take_requests_in(dir.path(), Utc::now()).unwrap().is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn expired_requests_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let stale = OpenRequest {
            id: Uuid::new_v4(),
            raw: "sqlly://open?connection=stale".into(),
            created_at: (Utc::now() - chrono::Duration::seconds(OPEN_REQUEST_TTL_SECS as i64 + 5))
                .to_rfc3339(),
        };
        let path = dir.path().join(format!("{}.json", stale.id));
        std::fs::write(&path, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();
        write_request_in(dir.path(), "sqlly://open?connection=fresh").unwrap();
        let taken = take_requests_in(dir.path(), Utc::now()).unwrap();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].raw, "sqlly://open?connection=fresh");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn instance_marker_ages_out() {
        let dir = tempfile::tempdir().unwrap();
        touch_instance_marker_in(dir.path()).unwrap();
        let now = std::time::SystemTime::now();
        assert!(instance_is_alive_in(
            dir.path(),
            now,
            StdDuration::from_secs(9)
        ));
        let path = dir.path().join("instance.lock");
        let stale = std::time::SystemTime::now() - StdDuration::from_secs(20);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(stale)).unwrap();
        assert!(!instance_is_alive_in(
            dir.path(),
            std::time::SystemTime::now(),
            StdDuration::from_secs(9)
        ));
    }
}
