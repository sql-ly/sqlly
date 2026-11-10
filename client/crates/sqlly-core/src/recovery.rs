use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single unsaved (or saved-but-modified) query tab captured for crash
/// recovery. The UI autosaves the [`RecoverySnapshot`] periodically; on the
/// next launch it restores any tabs whose `dirty` buffer differs from disk so
/// no in-progress editing is lost after a crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredTab {
    pub id: Uuid,
    pub title: String,
    /// The current editor buffer contents.
    pub sql: String,
    /// The on-disk file this tab is associated with, if any.
    pub file_path: Option<String>,
    /// Whether the buffer has unsaved changes relative to `file_path`.
    pub dirty: bool,
}

impl RecoveredTab {
    pub fn new(title: impl Into<String>, sql: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            sql: sql.into(),
            file_path: None,
            dirty: true,
        }
    }

    pub fn with_file(mut self, path: impl Into<String>) -> Self {
        self.file_path = Some(path.into());
        self
    }

    pub fn saved(mut self) -> Self {
        self.dirty = false;
        self
    }
}

/// A point-in-time capture of the open query tabs, suitable for serialising to
/// disk. `version` lets a future format migrate older snapshots safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverySnapshot {
    pub version: u32,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub tabs: Vec<RecoveredTab>,
}

impl RecoverySnapshot {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new(tabs: Vec<RecoveredTab>) -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            captured_at: chrono::Utc::now(),
            tabs,
        }
    }

    /// Serialises the snapshot to pretty JSON for the UI to write atomically.
    pub fn to_json(&self) -> Result<String, RecoveryError> {
        serde_json::to_string_pretty(self).map_err(|e| RecoveryError::Serialize(e.to_string()))
    }

    /// Parses a snapshot from JSON, rejecting an unknown future version so the
    /// app never silently mis-restores an incompatible format.
    pub fn from_json(json: &str) -> Result<Self, RecoveryError> {
        let snapshot: RecoverySnapshot =
            serde_json::from_str(json).map_err(|e| RecoveryError::Deserialize(e.to_string()))?;
        if snapshot.version > Self::CURRENT_VERSION {
            return Err(RecoveryError::UnsupportedVersion(snapshot.version));
        }
        Ok(snapshot)
    }

    /// The tabs worth restoring after a crash: those with unsaved changes.
    /// Clean tabs are reopened from their files by the UI, so they are omitted
    /// to avoid clobbering newer on-disk content.
    pub fn recoverable_tabs(&self) -> Vec<&RecoveredTab> {
        self.tabs.iter().filter(|t| t.dirty).collect()
    }

    pub fn has_recoverable_work(&self) -> bool {
        self.tabs.iter().any(|t| t.dirty)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryError {
    Serialize(String),
    Deserialize(String),
    UnsupportedVersion(u32),
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(e) => write!(f, "failed to serialize recovery snapshot: {e}"),
            Self::Deserialize(e) => write!(f, "failed to parse recovery snapshot: {e}"),
            Self::UnsupportedVersion(v) => {
                write!(f, "unsupported recovery snapshot version: {v}")
            }
        }
    }
}

impl std::error::Error for RecoveryError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = RecoverySnapshot::new(vec![
            RecoveredTab::new("Untitled-1", "SELECT 1"),
            RecoveredTab::new("report", "SELECT * FROM dbo.Orders")
                .with_file("/tmp/report.sql")
                .saved(),
        ]);

        let json = snapshot.to_json().unwrap();
        let restored = RecoverySnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
        assert_eq!(restored.version, RecoverySnapshot::CURRENT_VERSION);
    }

    #[test]
    fn recoverable_tabs_only_include_dirty_buffers() {
        let snapshot = RecoverySnapshot::new(vec![
            RecoveredTab::new("dirty", "SELECT 1"),
            RecoveredTab::new("clean", "SELECT 2")
                .with_file("/tmp/clean.sql")
                .saved(),
        ]);

        assert!(snapshot.has_recoverable_work());
        let recoverable = snapshot.recoverable_tabs();
        assert_eq!(recoverable.len(), 1);
        assert_eq!(recoverable[0].title, "dirty");
    }

    #[test]
    fn rejects_future_snapshot_version() {
        let json = r#"{"version":999,"captured_at":"2026-01-01T00:00:00Z","tabs":[]}"#;
        let err = RecoverySnapshot::from_json(json).unwrap_err();
        assert!(matches!(err, RecoveryError::UnsupportedVersion(999)));
    }

    #[test]
    fn no_recoverable_work_when_all_tabs_clean() {
        let snapshot = RecoverySnapshot::new(vec![RecoveredTab::new("a", "SELECT 1").saved()]);
        assert!(!snapshot.has_recoverable_work());
        assert!(snapshot.recoverable_tabs().is_empty());
    }
}
