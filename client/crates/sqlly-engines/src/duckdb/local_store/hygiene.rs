//! Disk hygiene across the notebook workspaces under one root: sizes, least
//! recently used eviction, and best-effort secure removal.

use super::{LAST_USED_FILE, LocalResult, LocalStoreError, WORKSPACE_FILE, validate_notebook_id};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One workspace directory under the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceUsage {
    pub notebook_id: String,
    pub dir: PathBuf,
    pub bytes: u64,
    pub last_used: SystemTime,
    /// A client in this process holds the database open.
    pub open: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvictionReport {
    pub evicted: Vec<String>,
    pub freed_bytes: u64,
    pub remaining_bytes: u64,
}

/// Total size of the regular files under `dir`; symlinks are neither
/// followed nor counted.
pub(super) fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_size(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

fn last_used(dir: &Path) -> SystemTime {
    std::fs::metadata(dir.join(LAST_USED_FILE))
        .or_else(|_| std::fs::metadata(dir))
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn is_open(dir: &Path) -> bool {
    super::super::file_in_use(&dir.join(WORKSPACE_FILE).to_string_lossy())
}

/// Every workspace under `root`, least recently used first. Entries that
/// are not plain directories named like a notebook id are ignored.
pub fn workspace_usages(root: &Path) -> LocalResult<Vec<WorkspaceUsage>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if validate_notebook_id(&name).is_err() {
            continue;
        }
        let dir = entry.path();
        out.push(WorkspaceUsage {
            bytes: dir_size(&dir),
            last_used: last_used(&dir),
            open: is_open(&dir),
            notebook_id: name,
            dir,
        });
    }
    out.sort_by(|a, b| {
        a.last_used
            .cmp(&b.last_used)
            .then_with(|| a.notebook_id.cmp(&b.notebook_id))
    });
    Ok(out)
}

/// Remove least recently used workspaces until everything under `root`
/// fits in `max_total_bytes`. Open workspaces and the ids in `keep` are
/// never removed, so the result can still be over the limit.
pub fn evict_lru(root: &Path, max_total_bytes: u64, keep: &[&str]) -> LocalResult<EvictionReport> {
    let usages = workspace_usages(root)?;
    let mut total: u64 = usages.iter().map(|u| u.bytes).sum();
    let mut report = EvictionReport::default();
    for usage in usages {
        if total <= max_total_bytes {
            break;
        }
        if usage.open || keep.contains(&usage.notebook_id.as_str()) {
            continue;
        }
        secure_remove_dir(&usage.dir);
        if usage.dir.exists() {
            continue;
        }
        total = total.saturating_sub(usage.bytes);
        report.freed_bytes += usage.bytes;
        report.evicted.push(usage.notebook_id);
    }
    report.remaining_bytes = total;
    Ok(report)
}

/// Remove one closed workspace. Returns whether it existed.
pub fn remove_workspace(root: &Path, notebook_id: &str) -> LocalResult<bool> {
    validate_notebook_id(notebook_id)?;
    let dir = root.join(notebook_id);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(LocalStoreError::PathRejected(
                super::PathRejection::NotARegularFile,
            ));
        }
        Err(_) => return Ok(false),
    }
    if is_open(&dir) {
        return Err(LocalStoreError::Io(format!(
            "the workspace for notebook '{notebook_id}' is still open"
        )));
    }
    secure_remove_dir(&dir);
    if dir.exists() {
        return Err(LocalStoreError::Io(format!(
            "the workspace for notebook '{notebook_id}' could not be removed"
        )));
    }
    Ok(true)
}

/// Overwrite each regular file with zeros, then delete the tree. Best
/// effort: copy-on-write file systems and SSD wear levelling can keep old
/// blocks, so encryption at rest is the real protection for sensitive data.
pub(super) fn secure_remove_dir(dir: &Path) {
    overwrite_files(dir);
    if let Err(error) = std::fs::remove_dir_all(dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, dir = %dir.display(), "could not remove a notebook workspace");
    }
}

fn overwrite_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => overwrite_files(&entry.path()),
            Ok(kind) if kind.is_file() => overwrite_file(&entry.path()),
            _ => {}
        }
    }
}

fn overwrite_file(path: &Path) {
    use std::io::Write;
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
        let mut left = file.metadata()?.len();
        let zeros = vec![0u8; 1024 * 1024];
        while left > 0 {
            let chunk = left.min(zeros.len() as u64) as usize;
            file.write_all(&zeros[..chunk])?;
            left -= chunk as u64;
        }
        file.sync_all()
    })();
    if let Err(error) = result {
        tracing::debug!(%error, "could not overwrite a notebook workspace file");
    }
}
