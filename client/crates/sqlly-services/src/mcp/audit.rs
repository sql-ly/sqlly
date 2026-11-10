use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "reason")]
pub enum AuditOutcome {
    Ok,
    Refused(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditApproval {
    Pending,
    Approved,
    Rejected,
    Expired,
    Executed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub ts: DateTime<Utc>,
    pub tool: String,
    pub connection_id: Option<Uuid>,
    pub connection_name: Option<String>,
    pub database: Option<String>,
    pub sql_hash: Option<String>,
    pub sql_redacted: Option<String>,
    pub duration_ms: Option<u64>,
    pub rows: Option<usize>,
    #[serde(flatten)]
    pub outcome: AuditOutcome,
    pub proposal_id: Option<Uuid>,
    pub approval: Option<AuditApproval>,
}

pub const AUDIT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
pub const AUDIT_KEEP_FILES: usize = 3;

#[cfg(not(target_arch = "wasm32"))]
pub fn audit_dir() -> PathBuf {
    if let Ok(path) = std::env::var("SQLLY_MCP_AUDIT_DIR") {
        return PathBuf::from(path);
    }
    if let Some(dir) = dirs::data_dir() {
        let dir = dir.join("SQLLY").join("mcp-audit");
        let _ = ensure_audit_dir(&dir);
        return dir;
    }
    PathBuf::from("mcp-audit")
}

#[cfg(not(target_arch = "wasm32"))]
fn ensure_audit_dir(dir: &Path) -> std::io::Result<()> {
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
pub fn append(record: &AuditRecord) -> Result<(), String> {
    append_in(&audit_dir(), record)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn append_in(dir: &Path, record: &AuditRecord) -> Result<(), String> {
    append_in_with_threshold(dir, record, AUDIT_ROTATE_BYTES)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn append_in_with_threshold(
    dir: &Path,
    record: &AuditRecord,
    rotate_bytes: u64,
) -> Result<(), String> {
    ensure_audit_dir(dir).map_err(|error| error.to_string())?;
    let path = dir.join("audit.jsonl");
    // A single write_all of one line under 4 KiB on an O_APPEND file is atomic
    // enough on all three platforms; the 2_000-char SQL cap keeps records under that.
    let line = serde_json::to_string(record).map_err(|error| error.to_string())?;
    let bytes = format!("{line}\n");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|error| error.to_string())?;
    std::io::Write::write_all(&mut file, bytes.as_bytes()).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    #[cfg(unix)]
    tighten_owner_only_file(&path);
    if file.metadata().map_err(|error| error.to_string())?.len() > rotate_bytes {
        rotate_audit_files(dir)?;
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn rotate_audit_files(dir: &Path) -> Result<(), String> {
    let oldest = dir.join(format!("audit.{}.jsonl", AUDIT_KEEP_FILES - 1));
    if oldest.exists() {
        std::fs::remove_file(&oldest).map_err(|error| error.to_string())?;
    }
    for n in (1..AUDIT_KEEP_FILES - 1).rev() {
        let from = dir.join(format!("audit.{n}.jsonl"));
        let to = dir.join(format!("audit.{}.jsonl", n + 1));
        if from.exists() {
            std::fs::rename(&from, &to).map_err(|error| error.to_string())?;
        }
    }
    let current = dir.join("audit.jsonl");
    let first = dir.join("audit.1.jsonl");
    if current.exists() {
        std::fs::rename(&current, &first).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read_recent(limit: usize) -> Result<Vec<AuditRecord>, String> {
    read_recent_in(&audit_dir(), limit)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read_recent_in(dir: &Path, limit: usize) -> Result<Vec<AuditRecord>, String> {
    let mut records = Vec::new();
    if limit == 0 {
        return Ok(records);
    }
    let mut paths = vec![dir.join("audit.jsonl")];
    for n in 1..AUDIT_KEEP_FILES {
        paths.push(dir.join(format!("audit.{n}.jsonl")));
    }
    for path in paths {
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let mut lines: Vec<&str> = text.lines().collect();
        lines.reverse();
        for line in lines {
            if records.len() >= limit {
                return Ok(records);
            }
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<AuditRecord>(line) {
                records.push(record);
            }
        }
    }
    Ok(records)
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(unix)]
fn tighten_owner_only_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o077));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(tool: &str) -> AuditRecord {
        AuditRecord {
            ts: Utc::now(),
            tool: tool.into(),
            connection_id: None,
            connection_name: None,
            database: None,
            sql_hash: None,
            sql_redacted: None,
            duration_ms: Some(1),
            rows: None,
            outcome: AuditOutcome::Ok,
            proposal_id: None,
            approval: None,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn append_creates_file_with_one_json_line() {
        let dir = tempfile::tempdir().unwrap();
        let record = sample_record("list_connections");
        append_in(dir.path(), &record).unwrap();
        let path = dir.path().join("audit.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        let parsed: AuditRecord = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed.tool, "list_connections");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[cfg(unix)]
    #[test]
    fn append_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        append_in(dir.path(), &sample_record("t")).unwrap();
        let path = dir.path().join("audit.jsonl");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn rotation_at_threshold_keeps_three() {
        let dir = tempfile::tempdir().unwrap();
        let threshold = 80;
        for i in 0..4 {
            let record = sample_record(&format!("tool_{i}"));
            append_in_with_threshold(dir.path(), &record, threshold).unwrap();
        }
        append_in(dir.path(), &sample_record("final")).unwrap();
        assert!(dir.path().join("audit.jsonl").exists());
        assert!(dir.path().join("audit.1.jsonl").exists());
        assert!(dir.path().join("audit.2.jsonl").exists());
        assert!(!dir.path().join("audit.3.jsonl").exists());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn read_recent_newest_first_across_files() {
        let dir = tempfile::tempdir().unwrap();
        append_in_with_threshold(dir.path(), &sample_record("first"), 10_000).unwrap();
        rotate_audit_files(dir.path()).unwrap();
        append_in_with_threshold(dir.path(), &sample_record("second"), 10_000).unwrap();
        let records = read_recent_in(dir.path(), 10).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].tool, "second");
        assert_eq!(records[1].tool, "first");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn read_recent_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        ensure_audit_dir(dir.path()).unwrap();
        let path = dir.path().join("audit.jsonl");
        let good = serde_json::to_string(&sample_record("ok")).unwrap();
        std::fs::write(&path, format!("not json\n{good}\n")).unwrap();
        let records = read_recent_in(dir.path(), 10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tool, "ok");
    }

    #[test]
    fn record_round_trips_outcome_and_approval() {
        let record = AuditRecord {
            ts: Utc::now(),
            tool: "propose_write".into(),
            connection_id: Some(Uuid::new_v4()),
            connection_name: Some("dev".into()),
            database: Some("app".into()),
            sql_hash: Some("abc".into()),
            sql_redacted: Some("UPDATE t".into()),
            duration_ms: Some(5),
            rows: None,
            outcome: AuditOutcome::Refused("rate limit".into()),
            proposal_id: Some(Uuid::new_v4()),
            approval: Some(AuditApproval::Rejected),
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: AuditRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, record);
    }
}
