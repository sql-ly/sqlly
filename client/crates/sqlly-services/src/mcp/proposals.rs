use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Open,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Opened,
    Executed,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: Uuid,
    pub kind: ProposalKind,
    pub connection: Uuid,
    pub database: Option<String>,
    pub sql: Option<String>,
    pub created_at: String,
    pub status: ProposalStatus,
    #[serde(default)]
    pub note: Option<String>,
}

pub const PROPOSAL_TTL_SECS: u64 = 600;

#[cfg(not(target_arch = "wasm32"))]
pub fn proposals_dir() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("SQLLY_MCP_PROPOSALS_DIR") {
        return std::path::PathBuf::from(path);
    }
    if let Some(dir) = dirs::data_dir() {
        let dir = dir.join("SQLLY").join("mcp-proposals");
        let _ = ensure_proposals_dir(&dir);
        return dir;
    }
    std::path::PathBuf::from("mcp-proposals")
}

#[cfg(not(target_arch = "wasm32"))]
fn ensure_proposals_dir(dir: &std::path::Path) -> std::io::Result<()> {
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
pub fn write_proposal(p: &Proposal) -> Result<std::path::PathBuf, String> {
    write_proposal_in(&proposals_dir(), p)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn write_proposal_in(
    dir: &std::path::Path,
    p: &Proposal,
) -> Result<std::path::PathBuf, String> {
    ensure_proposals_dir(dir).map_err(|error| error.to_string())?;
    let path = dir.join(format!("{}.json", p.id));
    write_proposal_to(&path, p)?;
    Ok(path)
}

#[cfg(not(target_arch = "wasm32"))]
fn write_proposal_to(path: &std::path::Path, p: &Proposal) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(p).map_err(|error| error.to_string())?;
    write_owner_only_atomic(path, &bytes).map_err(|error| error.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read_proposal(id: Uuid) -> Result<Option<Proposal>, String> {
    read_proposal_in(&proposals_dir(), id)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read_proposal_in(dir: &std::path::Path, id: Uuid) -> Result<Option<Proposal>, String> {
    let path = dir.join(format!("{id}.json"));
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn list_pending() -> Result<Vec<Proposal>, String> {
    list_pending_in(&proposals_dir())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn list_pending_in(dir: &std::path::Path) -> Result<Vec<Proposal>, String> {
    let mut proposals = Vec::new();
    if !dir.exists() {
        return Ok(proposals);
    }
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
        let proposal: Proposal = match serde_json::from_str(&text) {
            Ok(proposal) => proposal,
            Err(_) => continue,
        };
        if proposal.status == ProposalStatus::Pending {
            proposals.push(proposal);
        }
    }
    proposals.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(proposals)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn update_status(id: Uuid, status: ProposalStatus, note: Option<String>) -> Result<(), String> {
    update_status_in(&proposals_dir(), id, status, note)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn update_status_in(
    dir: &std::path::Path,
    id: Uuid,
    status: ProposalStatus,
    note: Option<String>,
) -> Result<(), String> {
    let path = dir.join(format!("{id}.json"));
    let mut proposal =
        read_proposal_in(dir, id)?.ok_or_else(|| "proposal not found".to_string())?;
    proposal.status = status;
    proposal.note = note;
    write_proposal_to(&path, &proposal)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn sweep_expired(now: DateTime<Utc>) -> usize {
    sweep_expired_in(&proposals_dir(), now)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn sweep_expired_in(dir: &std::path::Path, now: DateTime<Utc>) -> usize {
    let mut expired = 0usize;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut proposal) = serde_json::from_str::<Proposal>(&text) else {
            continue;
        };
        let Ok(created) = DateTime::parse_from_rfc3339(&proposal.created_at) else {
            continue;
        };
        let created = created.with_timezone(&Utc);
        let age = now.signed_duration_since(created);
        if proposal.status == ProposalStatus::Pending
            && age.num_seconds() > PROPOSAL_TTL_SECS as i64
        {
            proposal.status = ProposalStatus::Expired;
            let _ = write_proposal_to(&path, &proposal);
            #[cfg(not(target_arch = "wasm32"))]
            {
                let record = crate::mcp::audit::AuditRecord {
                    ts: now,
                    tool: "proposal".into(),
                    connection_id: Some(proposal.connection),
                    connection_name: None,
                    database: proposal.database.clone(),
                    sql_hash: proposal.sql.as_deref().map(sql_hash),
                    sql_redacted: proposal
                        .sql
                        .as_ref()
                        .map(|sql| truncate_redacted(sqlly_core::redact_sql(sql))),
                    duration_ms: None,
                    rows: None,
                    outcome: crate::mcp::audit::AuditOutcome::Ok,
                    proposal_id: Some(proposal.id),
                    approval: Some(crate::mcp::audit::AuditApproval::Expired),
                };
                let _ = crate::mcp::audit::append(&record);
            }
            expired += 1;
        }
        if now.signed_duration_since(created).num_hours() >= 24 {
            let _ = std::fs::remove_file(&path);
        }
    }
    expired
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
fn write_owner_only_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_proposals_dir(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("proposal.json");
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
        #[cfg(unix)]
        tighten_owner_only_file(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn sql_hash(sql: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sql.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn truncate_redacted(sql: String) -> String {
    if sql.len() > 2_000 {
        sql[..2_000].to_string()
    } else {
        sql
    }
}

pub fn new_proposal(
    kind: ProposalKind,
    connection: Uuid,
    database: Option<String>,
    sql: Option<String>,
) -> Proposal {
    Proposal {
        id: Uuid::new_v4(),
        kind,
        connection,
        database,
        sql,
        created_at: Utc::now().to_rfc3339(),
        status: ProposalStatus::Pending,
        note: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn write_list_update_pending() {
        let dir = tempfile::tempdir().unwrap();
        let proposal = new_proposal(
            ProposalKind::Write,
            Uuid::new_v4(),
            Some("app".into()),
            Some("UPDATE t SET x = 1".into()),
        );
        write_proposal_in(dir.path(), &proposal).unwrap();
        let pending = list_pending_in(dir.path()).unwrap();
        assert_eq!(pending.len(), 1);
        update_status_in(
            dir.path(),
            proposal.id,
            ProposalStatus::Executed,
            Some("ok".into()),
        )
        .unwrap();
        assert!(list_pending_in(dir.path()).unwrap().is_empty());
        let read = read_proposal_in(dir.path(), proposal.id).unwrap().unwrap();
        assert_eq!(read.status, ProposalStatus::Executed);
    }
}
