//! The notebook's local data in the GPUI shell: SQL cells that target
//! "Local (this notebook)", Materialize Locally from any SQL cell, cells
//! that join results from several connections in the notebook's DuckDB
//! workspace (`sqlly_services::local_analytics`), attachment cells, the
//! Local data rail page, cache storage per cell, and the row/byte caps on
//! remote cells.
//!
//! Local SQL cells are ordinary SQL cells with `spec.target = "local"`
//! (`sqlly_services::notebook::cells::local`); they run through the app's
//! regular execute path on the workspace's read-only DuckDB profile, so
//! results, spill and Cancel behave like any query.

// The browser build has no local database: most of this is native-only.
#![cfg_attr(target_arch = "wasm32", allow(dead_code, unused))]

pub mod cache;
pub mod caps;
#[cfg(not(target_arch = "wasm32"))]
pub mod materialize;
pub mod prefs;
mod render;
pub mod run;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod view_tests;

pub use render::AttachmentCx;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};

pub use prefs::LocalPrefs;

/// Name of the local target in pickers and badges.
pub const LOCAL_LABEL_KEY: &str = "notebook.local.target.label";

/// Where one local relation came from, ready to show.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvenanceView {
    pub kind: SourceKindView,
    /// The connection's (or file's) display name.
    pub label: String,
    pub environment: Option<String>,
    pub database: Option<String>,
    pub cell_id: Option<String>,
    pub run_id: Option<String>,
    pub sql_hash: Option<String>,
    pub source_path: Option<String>,
    pub upstream: Vec<String>,
    pub extracted_at: Option<DateTime<Utc>>,
    pub materialized_at: Option<DateTime<Utc>>,
    pub row_count: u64,
    pub complete: bool,
    /// Why the relation is partial, translated.
    pub partial: Option<String>,
    pub checksum: Option<String>,
    /// Name, local type, and whether values fell back to text.
    pub columns: Vec<(String, String, bool)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKindView {
    Query,
    File,
    Local,
}

/// One row of the Local data rail.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationRow {
    pub name: String,
    pub is_view: bool,
    pub rows: u64,
    pub columns: u64,
    pub bytes: u64,
    pub provenance: Option<ProvenanceView>,
}

/// A copy or import in progress, shown in the activity panel with Cancel.
pub struct LocalJob {
    pub id: u64,
    pub cell_id: Option<String>,
    pub relation: String,
    #[cfg(not(target_arch = "wasm32"))]
    pub progress: Arc<materialize::Progress>,
    #[cfg(not(target_arch = "wasm32"))]
    pub cancel: sqlly_services::local_analytics::CancelToken,
}

impl LocalJob {
    pub fn rows(&self) -> u64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.progress.rows()
        }
        #[cfg(target_arch = "wasm32")]
        {
            0
        }
    }
}

/// What a decision prompt in the notebook is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalPrompt {
    /// The first copy from a connection: allow results from it to be kept
    /// on this computer.
    Consent {
        profile_id: String,
        label: String,
        environment: sqlly_core::ConnectionEnvironment,
        then: PendingAction,
    },
    /// A local cell reads results from connections that are not copied (or
    /// are out of date) yet.
    Inputs {
        cell_id: String,
        inputs: Vec<InputLine>,
    },
    /// Materialize Locally: pick the relation name.
    Name { cell_id: String },
    /// The result on screen is incomplete: run the query again to copy
    /// every row.
    Refetch { cell_id: String, relation: String },
    /// Clear every relation of this notebook.
    Clear,
}

/// One input a local cell will copy in, as the prompt lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputLine {
    pub cell_name: String,
    pub connection: String,
    pub environment: sqlly_core::ConnectionEnvironment,
}

/// What continues after a consent prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingAction {
    Materialize { cell_id: String },
    RunLocal { cell_id: String },
}

/// A cell whose run stopped at its row or download cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Truncation {
    pub rows_kept: u64,
    pub by_bytes: bool,
}

/// What an attachment's last import found.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportSummary {
    pub relation: String,
    pub rows: u64,
    pub columns: Vec<(String, String, bool)>,
    pub partial: Option<String>,
    /// The file differed from the one attached (its content hash changed).
    pub changed: bool,
}

/// A validated inline image of an attachment cell.
#[derive(Clone)]
pub struct AttachmentImage {
    pub image: Arc<gpui::Image>,
    pub width: u32,
    pub height: u32,
}

/// Per-notebook local data state the view owns.
#[derive(Default)]
pub struct LocalData {
    pub prefs: LocalPrefs,
    pub relations: Vec<RelationRow>,
    pub disk_bytes: u64,
    pub encrypted: bool,
    /// The encryption switch is recreating the workspace.
    pub switching_encryption: bool,
    /// The last switch was refused because local tables exist.
    pub encryption_needs_clear: bool,
    /// Where workspace keys are kept; the OS keychain unless a test sets it.
    #[cfg(not(target_arch = "wasm32"))]
    pub keys: Option<Arc<dyn sqlly_services::notebook_security::workspace_keys::WorkspaceKeyStore>>,
    pub jobs: Vec<LocalJob>,
    pub prompt: Option<LocalPrompt>,
    pub error: Option<String>,
    /// The relation whose provenance popover is open in the rail.
    pub provenance_open: Option<String>,
    /// Cells whose last run stopped at a cap.
    pub truncated: HashMap<String, Truncation>,
    /// Cells whose next run ignores their caps (Fetch All).
    pub fetch_all: HashSet<String>,
    /// Local cells whose inputs the user agreed to copy this session.
    pub inputs_approved: BTreeSet<String>,
    /// The run id and finish time of each cell's last completed run.
    pub last_runs: HashMap<String, (String, DateTime<Utc>)>,
    pub imports: HashMap<String, ImportSummary>,
    pub images: HashMap<String, AttachmentImage>,
    /// Attachment pictures a Markdown cell asked for, loaded or tried once.
    pub image_loads: HashSet<String>,
    /// Running queries by results view, so concurrent cells cancel one by one.
    pub cell_queries: HashMap<gpui::EntityId, String>,
    /// The services each running query was started on, by task id, so
    /// Stop cancels a Via-Relay cell's query through its relay.
    pub query_routes: HashMap<String, sqlly_services::Services>,
    /// Restored cached outputs shown in place of a run, by cell.
    pub restored: HashSet<String>,
    /// When each restored output was stored (Unix ms), for its banner.
    pub restored_at: HashMap<String, i64>,
    pub next_job: u64,
    pub resolver: Option<run::QueryResolver>,
    pub ticker: Option<gpui::Task<()>>,
    #[cfg(not(target_arch = "wasm32"))]
    pub workspace: Option<sqlly_services::local_analytics::NotebookWorkspace>,
    #[cfg(not(target_arch = "wasm32"))]
    pub opening: bool,
    /// Callbacks waiting for the workspace to open.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) waiters: Vec<run::WorkspaceWaiter>,
}

impl LocalData {
    pub fn new() -> Self {
        Self {
            prefs: LocalPrefs::load(),
            ..Default::default()
        }
    }

    pub fn relation(&self, name: &str) -> Option<&RelationRow> {
        self.relations
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(name))
    }

    /// The relation materialized from `cell_id`, if any.
    pub fn relation_from_cell(&self, cell_id: &str) -> Option<&RelationRow> {
        self.relations.iter().find(|r| {
            r.provenance
                .as_ref()
                .is_some_and(|p| p.cell_id.as_deref() == Some(cell_id))
        })
    }

    pub fn quota_bytes(&self) -> u64 {
        self.prefs.max_disk_mb.saturating_mul(1024 * 1024)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn provenance_view(
    record: &sqlly_services::local_analytics::ProvenanceRecord,
) -> ProvenanceView {
    use sqlly_services::local_analytics::SourceKind;
    let parse = |text: &str| {
        DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    };
    ProvenanceView {
        kind: match record.source_kind {
            SourceKind::Query => SourceKindView::Query,
            SourceKind::File => SourceKindView::File,
            SourceKind::Local => SourceKindView::Local,
        },
        label: match record.source_kind {
            SourceKind::Local => crate::localization::text(LOCAL_LABEL_KEY),
            _ => record.source_label.clone(),
        },
        environment: record.environment.clone(),
        database: record.database.clone(),
        cell_id: record.cell_id.clone(),
        run_id: record.run_id.clone(),
        sql_hash: record.source_sql_hash.clone(),
        source_path: record.source_path.clone(),
        upstream: record.upstream.clone(),
        extracted_at: parse(&record.extracted_at),
        materialized_at: parse(&record.materialized_at),
        row_count: record.row_count,
        complete: record.complete,
        partial: record.partial_reason.as_ref().map(partial_text),
        checksum: record.checksum.clone(),
        columns: record
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.local_type.clone(), c.fell_back_to_text))
            .collect(),
    }
}

/// Why a relation is partial, in the active language. A source detail
/// saved as a catalog message is worded here; older records kept text.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn partial_text(reason: &sqlly_services::local_analytics::PartialReason) -> String {
    use sqlly_services::local_analytics::{PartialReason, partial_message};
    let worded = match reason {
        PartialReason::SourceIncomplete { detail } => {
            match serde_json::from_str::<sqlly_core::StoredMessage>(detail) {
                Ok(stored) => PartialReason::SourceIncomplete {
                    detail: crate::localization::stored_message(&stored, &stored.key),
                },
                Err(_) => reason.clone(),
            }
        }
        _ => reason.clone(),
    };
    crate::localization::message(partial_message(&worded))
}

/// What a cell's results say when its run stopped at a cap.
pub fn caps_message(truncation: Truncation) -> String {
    let key = match (truncation.by_bytes, truncation.rows_kept == 1) {
        (true, true) => "notebook.local.caps.stopped_bytes_one",
        (true, false) => "notebook.local.caps.stopped_bytes",
        (false, true) => "notebook.local.caps.stopped_rows_one",
        (false, false) => "notebook.local.caps.stopped_rows",
    };
    crate::localization::format(key, &[("count", count_text(truncation.rows_kept))])
}

/// "2 min ago" style age of `at` relative to `now`.
pub fn age_text(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - at).num_seconds().max(0);
    if secs < 60 {
        crate::localization::text("notebook.local.age.just_now")
    } else if secs < 3600 {
        crate::localization::format("notebook.local.age.minutes", &[("count", secs / 60)])
    } else if secs < 86_400 {
        crate::localization::format("notebook.local.age.hours", &[("count", secs / 3600)])
    } else {
        crate::localization::format("notebook.local.age.days", &[("count", secs / 86_400)])
    }
}

/// Thousands-separated count for badges.
pub fn count_text(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Bytes as KB/MB/GB for the rail.
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_sizes_and_ages_read_naturally() {
        assert_eq!(count_text(48_210), "48,210");
        assert_eq!(count_text(7), "7");
        assert_eq!(count_text(1_000_000), "1,000,000");
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(3 * 1024 * 1024), "3.0 MB");
        let now = Utc::now();
        assert_eq!(
            age_text(now - chrono::Duration::minutes(2), now),
            crate::localization::format("notebook.local.age.minutes", &[("count", 2)])
        );
        assert_eq!(
            age_text(now, now),
            crate::localization::text("notebook.local.age.just_now")
        );
    }
}
