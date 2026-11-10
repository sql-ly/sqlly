//! The merge: which copy of each item wins, and what that means for this
//! Mac. Pure: no files, no clock.
//!
//! Each item's newest change wins (`modified_at`); on a tie a delete beats an
//! edit, and between two edits the larger content hash wins, so every Mac
//! picks the same winner from the same inputs. Deletes are tombstones that
//! are never dropped, so a Mac that was offline for months cannot bring a
//! deleted item back with its stale copy.
//!
//! This Mac's side is described by what it holds now ([`LocalSnapshot`])
//! plus what it held after its last sync ([`SyncState`]): an item whose
//! content differs from the recorded hash changed here since then and is
//! stamped "now"; one that disappeared was deleted here and becomes a
//! tombstone. Items this Mac has not touched since its last sync add
//! nothing, so the merged view of them comes from the Macs' files as is.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::document::{SyncDocument, SyncItem, payload_hash};

/// What this Mac recorded about one item after its last sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateEntry {
    /// Hash of this Mac's copy right after that sync ("" when absent).
    pub local_hash: String,
    /// Hash of the merged copy that sync settled on ("" for a tombstone).
    pub synced_hash: String,
    pub modified_at: i64,
    #[serde(default)]
    pub deleted: bool,
}

/// This Mac's sync record, kept beside its settings (never synced).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncState {
    /// This Mac's id: the name of its file in iCloud.
    pub device_id: String,
    #[serde(default)]
    pub items: BTreeMap<String, StateEntry>,
}

/// One item as this Mac holds it now.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalItem {
    /// Synced content.
    Content(Value),
    /// Exists here but is not synced (too large, say): never sent, never
    /// overwritten, never deleted by sync.
    Opaque,
}

/// Everything this Mac holds now, by item id, plus which domains were read
/// successfully. Items of a domain that could not be read are neither sent
/// nor changed: an unreadable store must not look like "everything was
/// deleted".
#[derive(Debug, Clone, Default)]
pub struct LocalSnapshot {
    pub items: BTreeMap<String, LocalItem>,
    pub covered: BTreeSet<String>,
}

impl LocalSnapshot {
    pub fn covers(&self, id: &str) -> bool {
        self.covered.contains(&domain_of(id))
    }

    pub fn content(&self, id: &str) -> Option<&Value> {
        match self.items.get(id) {
            Some(LocalItem::Content(value)) => Some(value),
            _ => None,
        }
    }

    fn local_hash(&self, id: &str) -> Option<String> {
        self.content(id).map(payload_hash)
    }
}

/// The domain an item id belongs to: each preference file is its own
/// domain (one unreadable file must not hold up the others); everything
/// else groups by the id's kind prefix (`connection:`, `library:`, …).
pub fn domain_of(id: &str) -> String {
    if id.starts_with("preference:") {
        return id.to_string();
    }
    match id.find(':') {
        Some(at) => id[..=at].to_string(),
        None => id.to_string(),
    }
}

/// This Mac's contribution to the merge (see the module docs).
pub fn local_entries(
    snapshot: &LocalSnapshot,
    state: &SyncState,
    remote_ids: &BTreeSet<String>,
    now: i64,
) -> BTreeMap<String, SyncItem> {
    let mut out = BTreeMap::new();
    let mut ids: BTreeSet<&String> = snapshot.items.keys().collect();
    ids.extend(state.items.keys());
    for id in ids {
        if !snapshot.covers(id) {
            continue;
        }
        let recorded = state.items.get(id);
        match snapshot.items.get(id) {
            Some(LocalItem::Opaque) => {}
            Some(LocalItem::Content(payload)) => {
                let hash = payload_hash(payload);
                match recorded {
                    Some(entry) if !entry.deleted && entry.local_hash == hash => {
                        // Untouched since the last sync. Re-send it only if
                        // no Mac's file carries it any more.
                        if !remote_ids.contains(id) {
                            out.insert(
                                id.clone(),
                                SyncItem::live(entry.modified_at, payload.clone()),
                            );
                        }
                    }
                    Some(_) => {
                        out.insert(id.clone(), SyncItem::live(now, payload.clone()));
                    }
                    None => {
                        // Never synced from here. When iCloud already has
                        // this item, the synced copy wins (a Mac joining
                        // later takes the shared copy); otherwise it is new.
                        let stamp = if remote_ids.contains(id) { 0 } else { now };
                        out.insert(id.clone(), SyncItem::live(stamp, payload.clone()));
                    }
                }
            }
            None => match recorded {
                Some(entry) if !entry.deleted => {
                    out.insert(id.clone(), SyncItem::tombstone(now));
                }
                // Deleted here before; re-send the tombstone only if no
                // Mac's file carries it any more.
                Some(entry) if !remote_ids.contains(id) => {
                    out.insert(id.clone(), SyncItem::tombstone(entry.modified_at));
                }
                Some(_) | None => {}
            },
        }
    }
    out
}

/// Whether `a` beats `b`.
pub fn wins(a: &SyncItem, b: &SyncItem) -> bool {
    let key = |item: &SyncItem| (item.modified_at, item.deleted, item.hash());
    key(a) > key(b)
}

/// The winning copy of every item across this Mac and every Mac's file.
pub fn merge(
    local: &BTreeMap<String, SyncItem>,
    remotes: &[SyncDocument],
) -> BTreeMap<String, SyncItem> {
    let mut merged: BTreeMap<String, SyncItem> = BTreeMap::new();
    let candidates = remotes
        .iter()
        .flat_map(|document| document.items.iter())
        .chain(local.iter());
    for (id, item) in candidates {
        match merged.get(id) {
            Some(current) if !wins(item, current) => {}
            _ => {
                merged.insert(id.clone(), item.clone());
            }
        }
    }
    merged
}

/// A change sync makes on this Mac.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalChange {
    Upsert { id: String, payload: Value },
    Delete { id: String },
}

impl LocalChange {
    pub fn id(&self) -> &str {
        match self {
            Self::Upsert { id, .. } | Self::Delete { id } => id,
        }
    }
}

/// What applying `merged` changes here. Domains this Mac could not read
/// and items it keeps unsynced are left alone. An item already settled at
/// this merged copy (this Mac's stored form can differ in field order or
/// defaults) is not re-applied.
pub fn plan_local_changes(
    snapshot: &LocalSnapshot,
    state: &SyncState,
    merged: &BTreeMap<String, SyncItem>,
) -> Vec<LocalChange> {
    let mut changes = Vec::new();
    for (id, item) in merged {
        if !snapshot.covers(id) || matches!(snapshot.items.get(id), Some(LocalItem::Opaque)) {
            continue;
        }
        let local_hash = snapshot.local_hash(id);
        if item.deleted {
            if local_hash.is_some() {
                changes.push(LocalChange::Delete { id: id.clone() });
            }
            continue;
        }
        let Some(payload) = &item.payload else {
            continue;
        };
        let merged_hash = payload_hash(payload);
        let settled = match (&local_hash, state.items.get(id)) {
            (Some(local), _) if *local == merged_hash => true,
            (Some(local), Some(entry)) => {
                !entry.deleted && entry.synced_hash == merged_hash && entry.local_hash == *local
            }
            _ => false,
        };
        if !settled {
            changes.push(LocalChange::Upsert {
                id: id.clone(),
                payload: payload.clone(),
            });
        }
    }
    changes
}

/// The sync record after a pass. `after` is this Mac re-read once the
/// changes were applied; `failed` lists changes that could not be applied
/// (their old record is kept, so the next pass retries them).
pub fn next_state(
    previous: &SyncState,
    after: &LocalSnapshot,
    merged: &BTreeMap<String, SyncItem>,
    failed: &BTreeSet<String>,
) -> SyncState {
    let mut items = previous.items.clone();
    for (id, item) in merged {
        if !after.covers(id) || failed.contains(id) {
            continue;
        }
        if matches!(after.items.get(id), Some(LocalItem::Opaque)) {
            continue;
        }
        items.insert(
            id.clone(),
            StateEntry {
                local_hash: after.local_hash(id).unwrap_or_default(),
                synced_hash: item.hash(),
                modified_at: item.modified_at,
                deleted: item.deleted,
            },
        );
    }
    SyncState {
        device_id: previous.device_id.clone(),
        items,
    }
}
