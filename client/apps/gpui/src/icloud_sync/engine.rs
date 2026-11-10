//! One sync pass: read every Mac's file, merge with this Mac, apply, write
//! this Mac's file, save the record.
//!
//! The order keeps every failure safe. Nothing is applied until the
//! record and the folder listing have been read; a file that cannot be used
//! is skipped whole; this Mac's file is written only after applying, and
//! the record last, so a crash in between just repeats work on the next
//! pass.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::document::{SkippedFile, SyncDocument, read_devices_folder, write_device_file};
use super::local::{ApplyReport, LocalPlaces, apply, collect};
use super::merge::{SyncState, local_entries, merge, next_state, plan_local_changes};

/// Name of this Mac's sync record in the settings folder. Never synced.
pub const STATE_FILE: &str = "icloud-sync-state.json";

/// What one pass did.
#[derive(Debug, Default)]
pub struct PassReport {
    pub apply: ApplyReport,
    /// Whether this Mac's file in iCloud was rewritten.
    pub wrote_device_file: bool,
    /// Other Macs' files that were not used.
    pub skipped: Vec<SkippedFile>,
    /// Files iCloud has not downloaded yet.
    pub pending: Vec<PathBuf>,
}

/// Why a pass stopped before changing anything (or, for
/// [`PassError::WriteFailed`], after applying incoming changes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassError {
    /// This Mac's sync record exists but cannot be read.
    StateUnreadable { path: PathBuf, detail: String },
    /// The iCloud folder cannot be listed.
    FolderUnreadable { path: PathBuf, detail: String },
    /// Writing this Mac's file or record failed.
    WriteFailed { path: PathBuf, detail: String },
}

/// Inputs to a pass.
pub struct PassInput<'a> {
    pub places: LocalPlaces<'a>,
    /// The iCloud folder holding each Mac's file.
    pub devices_dir: PathBuf,
    /// This Mac's sync record.
    pub state_path: PathBuf,
    /// Milliseconds since the Unix epoch.
    pub now: i64,
}

/// Load the record; a missing one starts fresh with a new device id.
pub fn load_state(path: &std::path::Path) -> Result<SyncState, PassError> {
    match crate::fs_safe::load_json_store::<SyncState>(path) {
        crate::fs_safe::StoreLoad::Missing => Ok(SyncState {
            device_id: uuid::Uuid::new_v4().to_string(),
            items: Default::default(),
        }),
        crate::fs_safe::StoreLoad::Loaded(state)
            if uuid::Uuid::parse_str(&state.device_id).is_ok() =>
        {
            Ok(state)
        }
        crate::fs_safe::StoreLoad::Loaded(_) => Err(PassError::StateUnreadable {
            path: path.to_path_buf(),
            detail: "missing device id".to_string(),
        }),
        crate::fs_safe::StoreLoad::Corrupt { error } => Err(PassError::StateUnreadable {
            path: path.to_path_buf(),
            detail: crate::localization::message(&error),
        }),
    }
}

pub fn run_pass(input: &PassInput<'_>) -> Result<PassReport, PassError> {
    let state = load_state(&input.state_path)?;
    let remote =
        read_devices_folder(&input.devices_dir).map_err(|error| PassError::FolderUnreadable {
            path: input.devices_dir.clone(),
            detail: error.to_string(),
        })?;
    let own_previous: Option<&SyncDocument> = remote
        .documents
        .iter()
        .find(|d| d.device_id == state.device_id);

    let before = collect(&input.places);
    let remote_ids: BTreeSet<String> = remote
        .documents
        .iter()
        .flat_map(|d| d.items.keys().cloned())
        .collect();
    let local = local_entries(&before, &state, &remote_ids, input.now);
    let merged = merge(&local, &remote.documents);
    let changes = plan_local_changes(&before, &state, &merged);
    let applied = apply(&input.places, &before, &state, &changes, input.now);

    let mut report = PassReport {
        skipped: remote.skipped.clone(),
        pending: remote.pending.clone(),
        ..PassReport::default()
    };

    // Conflicted copies are new files: pick them up now so they go out in
    // this pass rather than the next.
    let after = collect(&input.places);
    let (merged, after) = if applied.conflicted_copies.is_empty() {
        (merged, after)
    } else {
        let next = next_state(&state, &after, &merged, &applied.failed);
        let extra = local_entries(&after, &next, &remote_ids, input.now);
        let mut merged = merged;
        for (id, item) in extra {
            merged.entry(id).or_insert(item);
        }
        (merged, after)
    };

    if own_previous.map(|d| &d.items) != Some(&merged) {
        let document = SyncDocument::new(&state.device_id, input.now, merged.clone());
        write_device_file(&input.devices_dir, &document).map_err(|error| {
            PassError::WriteFailed {
                path: input.devices_dir.clone(),
                detail: error.to_string(),
            }
        })?;
        report.wrote_device_file = true;
    }

    let next = next_state(&state, &after, &merged, &applied.failed);
    if next != state {
        let bytes = serde_json::to_vec_pretty(&next).map_err(|error| PassError::WriteFailed {
            path: input.state_path.clone(),
            detail: error.to_string(),
        })?;
        crate::fs_safe::write_atomic(&input.state_path, &bytes).map_err(|error| {
            PassError::WriteFailed {
                path: input.state_path.clone(),
                detail: error.to_string(),
            }
        })?;
    }
    report.apply = applied;
    Ok(report)
}
