//! Notebook trust (spec 12.2 "trust level per imported notebook", NB-09).
//!
//! A notebook is trusted on this machine only when it was created here or
//! the user granted trust, and only while its executable parts hash to what
//! was trusted. The hash ([`executable_hash`]) covers everything that
//! decides what runs where: cell ids, names, kinds and order, SQL and other
//! non-Markdown sources, connection slots and their profile mappings,
//! execution policies, session groups, specs, parameters, run mode, data
//! retention and unknown fields. Markdown prose and titles are not
//! executable, so editing them keeps trust.
//!
//! Opening a notebook only calls [`assess`], which never runs anything. An
//! untrusted notebook can be read and edited; every run needs a [`grant`]
//! first, and the grant records which connection slots it authorized and
//! the profile each was mapped to, so a later remap needs a new grant.
//! In-app saves move a trusted record to the new hash through
//! [`TrustStore::record_in_app_save`]; a change made outside the app
//! (the file on disk hashes differently) drops the notebook to untrusted.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlly_core::DisplayMessage;

use super::messages::message;
use crate::notebook::document::Sorted;
use crate::notebook::{CellKind, NotebookDoc};

/// Format version of the trust store file.
pub const STORE_VERSION: u32 = 1;
/// Trust records kept per notebook id (different trusted contents, e.g. two
/// branches of the same notebook); the oldest is dropped beyond this.
pub const MAX_RECORDS_PER_NOTEBOOK: usize = 8;
/// The slot key for cells that run on the host's current connection.
pub const CURRENT_CONNECTION: &str = "";
/// Days a created-here record of a notebook that was never saved to a file
/// is kept when no recovery copy holds it.
pub const ABANDONED_AFTER_DAYS: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Created here or fully trusted: cells may run on the authorized
    /// connections, write cells after elevation.
    Trusted,
    /// Nothing runs until the user grants trust.
    Untrusted,
    /// Read-only cells may run on the authorized connections; write cells,
    /// schedules and app publishing are refused.
    Restricted,
}

/// How a record came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustOrigin {
    CreatedHere,
    Granted,
}

/// Why [`assess`] reached its level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustReason {
    CreatedHere,
    Granted,
    /// Never created or trusted on this machine.
    NotTrustedHere,
    /// Trusted before, but its executable parts changed outside the app.
    ChangedOutsideApp,
}

impl TrustReason {
    pub fn trust_reason_key(self) -> &'static str {
        match self {
            TrustReason::CreatedHere => "notebook.security.trust.created_here",
            TrustReason::Granted => "notebook.security.trust.granted",
            TrustReason::NotTrustedHere => "notebook.security.trust.not_trusted_here",
            TrustReason::ChangedOutsideApp => "notebook.security.trust.changed_outside_app",
        }
    }

    pub fn message(self) -> DisplayMessage {
        let text = match self {
            TrustReason::CreatedHere => "You created this notebook on this computer.",
            TrustReason::Granted => "You trusted this notebook on this computer.",
            TrustReason::NotTrustedHere => {
                "This notebook came from somewhere else. Nothing runs until you trust it and choose the connections it may use."
            }
            TrustReason::ChangedOutsideApp => {
                "This notebook's queries or connections changed outside SQLLY since you trusted it. Review it and trust it again to run it."
            }
        };
        message(self.trust_reason_key(), text, &[])
    }
}

/// One trusted (notebook id, executable hash) pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustRecord {
    pub notebook_id: String,
    pub level: TrustLevel,
    pub executable_hash: String,
    pub origin: TrustOrigin,
    /// Authorized connection slots and the profile each was mapped to when
    /// authorized ([`CURRENT_CONNECTION`] for the host's connection).
    pub connections: BTreeMap<String, Option<String>>,
    pub allow_writes: bool,
    pub updated_at: DateTime<Utc>,
    /// SHA-256 of the path the notebook was trusted at, so a moved copy can
    /// be noted; never decides trust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_hash: Option<String>,
    /// Every slot the notebook declared when this record was written, so an
    /// in-app save can tell a slot the user just added (authorized) from
    /// one the grant left out (still pending).
    #[serde(default)]
    pub known_slots: BTreeSet<String>,
    /// The key this notebook's history, local data, file approvals and AI
    /// consent are kept under. Chosen when trust is first recorded and kept
    /// across in-app saves, so a file that only borrows another notebook's
    /// id never inherits its data. Empty in records written before scopes
    /// existed, which use the notebook id ([`TrustRecord::data_scope`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
}

impl TrustRecord {
    pub fn data_scope(&self) -> &str {
        if self.scope.is_empty() {
            &self.notebook_id
        } else {
            &self.scope
        }
    }
}

/// What the app shows and enforces for an opened notebook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustDecision {
    pub notebook_id: String,
    pub level: TrustLevel,
    pub reason: TrustReason,
    pub executable_hash: String,
    /// Authorized slots with the profile each was authorized for. Empty
    /// when untrusted.
    pub connections: BTreeMap<String, Option<String>>,
    pub allow_writes: bool,
    /// Slots the notebook declares that this trust does not cover (the
    /// remap / authorize step lists them).
    pub pending_connections: Vec<String>,
    /// The notebook was trusted at another path.
    pub moved: bool,
    /// The data scope of the trust record this decision came from
    /// ([`TrustRecord::data_scope`]); `None` when untrusted, so nothing is
    /// read from (or kept for) a notebook nobody trusted here.
    pub scope: Option<String>,
}

impl TrustDecision {
    /// An untrusted decision for a document no store has seen.
    pub fn untrusted(doc: &NotebookDoc) -> TrustDecision {
        TrustDecision {
            notebook_id: doc.notebook_id.clone(),
            level: TrustLevel::Untrusted,
            reason: TrustReason::NotTrustedHere,
            executable_hash: executable_hash(doc),
            connections: BTreeMap::new(),
            allow_writes: false,
            pending_connections: declared_slots(doc).into_iter().collect(),
            moved: false,
            scope: None,
        }
    }

    /// Whether `slot` is authorized and still mapped to `profile_id`.
    pub fn authorizes(&self, slot: &str, profile_id: Option<&str>) -> bool {
        if self.level == TrustLevel::Untrusted {
            return false;
        }
        match self.connections.get(slot) {
            Some(authorized) => authorized.as_deref() == profile_id || slot == CURRENT_CONNECTION,
            None => false,
        }
    }

    /// Whether the trust banner should be shown.
    pub fn needs_banner(&self) -> bool {
        self.level != TrustLevel::Trusted || !self.pending_connections.is_empty()
    }
}

/// What the user chose in the trust dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// [`TrustLevel::Trusted`] or [`TrustLevel::Restricted`].
    pub level: TrustLevel,
    /// Slots to authorize (declared connection ref ids, or
    /// [`CURRENT_CONNECTION`]).
    pub connections: BTreeSet<String>,
    /// Ignored (false) for restricted grants.
    pub allow_writes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    /// Untrusted is not a grant; use [`TrustStore::revoke`].
    NotAGrant,
    UnknownConnection {
        slot: String,
    },
    MissingNotebookId,
}

impl GrantError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            GrantError::NotAGrant => message(
                "notebook.security.grant.not_a_grant",
                "Choose Trust or Read-only trust to allow runs.",
                &[],
            ),
            GrantError::UnknownConnection { slot } => message(
                "notebook.security.grant.unknown_connection",
                "The notebook has no connection named \"{slot}\".",
                &[("slot", slot.clone())],
            ),
            GrantError::MissingNotebookId => message(
                "notebook.security.grant.missing_notebook_id",
                "This notebook has no id, so trust can't be remembered. Save it as a .sqllynb file first.",
                &[],
            ),
        }
    }
}

/// All trust records on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustStore {
    pub version: u32,
    #[serde(default)]
    pub notebooks: BTreeMap<String, Vec<TrustRecord>>,
}

impl Default for TrustStore {
    fn default() -> Self {
        TrustStore {
            version: STORE_VERSION,
            notebooks: BTreeMap::new(),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn path_hash(path: Option<&std::path::Path>) -> Option<String> {
    path.map(|p| hex(&Sha256::digest(p.to_string_lossy().as_bytes())))
}

/// Connection slots the document can run on: declared refs, plus the
/// host's current connection when a runnable cell has no slot.
pub fn declared_slots(doc: &NotebookDoc) -> BTreeSet<String> {
    let mut slots: BTreeSet<String> = doc.connection_refs.iter().map(|c| c.id.clone()).collect();
    for cell in &doc.cells {
        match doc.effective_connection_ref(cell) {
            Some(slot) => {
                slots.insert(slot.to_string());
            }
            None if cell.exec_class() == crate::notebook::ExecClass::Remote => {
                slots.insert(CURRENT_CONNECTION.to_string());
            }
            None => {}
        }
    }
    slots
}

/// The executable view of a document, hashed by [`executable_hash`].
fn executable_view(doc: &NotebookDoc) -> Value {
    let cells: Vec<Value> = doc
        .cells
        .iter()
        .map(|cell| {
            let mut view = json!({
                "id": cell.id,
                "name": cell.name,
                "kind": cell.kind.as_str(),
            });
            if cell.kind != CellKind::Markdown {
                view["source"] = json!(cell.source);
                view["connection_ref"] = json!(cell.connection_ref);
                view["execution_policy"] =
                    serde_json::to_value(&cell.execution_policy).unwrap_or(Value::Null);
                view["session_group"] = json!(cell.session_group);
                view["spec"] = cell.spec.clone();
                let mut extra = cell.extra.clone();
                for key in crate::notebook::editing::VIEW_KEYS {
                    extra.remove(key);
                }
                extra.remove(crate::notebook::display::DISPLAY_KEY);
                view["extra"] = serde_json::to_value(&extra).unwrap_or(Value::Null);
            }
            view
        })
        .collect();
    json!({
        "notebook_id": doc.notebook_id,
        "connection_refs": serde_json::to_value(&doc.connection_refs).unwrap_or(Value::Null),
        "parameters": serde_json::to_value(&doc.parameters).unwrap_or(Value::Null),
        "settings": serde_json::to_value(crate::notebook::display::without_display(&doc.settings))
            .unwrap_or(Value::Null),
        "data_retention": serde_json::to_value(&doc.data_retention).unwrap_or(Value::Null),
        "extra": serde_json::to_value(&doc.extra).unwrap_or(Value::Null),
        "cells": cells,
    })
}

/// SHA-256 (hex) over the executable parts of `doc`, independent of field
/// order and formatting.
pub fn executable_hash(doc: &NotebookDoc) -> String {
    let text = serde_json::to_string(&Sorted(&executable_view(doc))).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(b"sqlly.notebook.exec.v1\n");
    hasher.update(text.as_bytes());
    hex(&hasher.finalize())
}

fn profile_of(doc: &NotebookDoc, slot: &str) -> Option<String> {
    doc.connection(slot).and_then(|c| c.profile_id.clone())
}

impl TrustStore {
    fn records(&self, notebook_id: &str) -> &[TrustRecord] {
        self.notebooks
            .get(notebook_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn upsert(&mut self, record: TrustRecord) {
        let list = self
            .notebooks
            .entry(record.notebook_id.clone())
            .or_default();
        list.retain(|r| r.executable_hash != record.executable_hash);
        list.push(record);
        if list.len() > MAX_RECORDS_PER_NOTEBOOK {
            list.sort_by_key(|r| r.updated_at);
            let excess = list.len() - MAX_RECORDS_PER_NOTEBOOK;
            list.drain(..excess);
        }
    }

    /// Trust a notebook the user just created in the app: every slot,
    /// writes allowed (still behind per-cell elevation).
    pub fn record_created(
        &mut self,
        doc: &NotebookDoc,
        path: Option<&std::path::Path>,
        now: DateTime<Utc>,
    ) -> TrustDecision {
        let connections = declared_slots(doc)
            .into_iter()
            .map(|slot| {
                let profile = profile_of(doc, &slot);
                (slot, profile)
            })
            .collect();
        self.upsert(TrustRecord {
            notebook_id: doc.notebook_id.clone(),
            level: TrustLevel::Trusted,
            executable_hash: executable_hash(doc),
            origin: TrustOrigin::CreatedHere,
            connections,
            allow_writes: true,
            updated_at: now,
            path_hash: path_hash(path),
            known_slots: declared_slots(doc),
            scope: String::new(),
        });
        assess(self, doc, path)
    }

    /// The scope a grant for `notebook_id` at `path` uses: that of a record
    /// trusted at the same path (the same file, edited elsewhere), else one
    /// derived from the file's real path (stable across one-run grants),
    /// else a new one. A file reusing another notebook's id elsewhere
    /// starts empty.
    pub fn scope_for_grant(&self, notebook_id: &str, path: Option<&std::path::Path>) -> String {
        let here = path_hash(path);
        if let Some(record) = self
            .records(notebook_id)
            .iter()
            .filter(|r| here.is_some() && r.path_hash == here)
            .max_by_key(|r| r.updated_at)
        {
            return record.data_scope().to_string();
        }
        match path {
            Some(path) => {
                let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                let digest = Sha256::digest(real.to_string_lossy().as_bytes());
                format!("p_{}", hex(&digest[..16]))
            }
            None => {
                let mut bytes = [0u8; 16];
                crate::notebook::IdSource::fill(&mut crate::notebook::RandomIds, &mut bytes);
                format!("s_{}", hex(&bytes))
            }
        }
    }

    /// The user saved (or remapped) a notebook in the app. A record trusted
    /// at `previous_hash` moves to the document's new hash and authorizes
    /// any slot the user added. Returns false (nothing changes) when the
    /// notebook was not trusted at `previous_hash`.
    pub fn record_in_app_save(
        &mut self,
        previous_hash: &str,
        doc: &NotebookDoc,
        path: Option<&std::path::Path>,
        now: DateTime<Utc>,
    ) -> bool {
        let Some(previous) = self
            .records(&doc.notebook_id)
            .iter()
            .find(|r| r.executable_hash == previous_hash && r.level != TrustLevel::Untrusted)
            .cloned()
        else {
            return false;
        };
        let declared = declared_slots(doc);
        let mut connections = BTreeMap::new();
        for slot in &declared {
            // Authorized slots follow an in-app remap; slots the user added
            // here are theirs; slots the grant left out stay pending.
            if previous.connections.contains_key(slot) || !previous.known_slots.contains(slot) {
                connections.insert(slot.clone(), profile_of(doc, slot));
            }
        }
        let new_hash = executable_hash(doc);
        if let Some(list) = self.notebooks.get_mut(&doc.notebook_id) {
            list.retain(|r| r.executable_hash != previous_hash);
        }
        self.upsert(TrustRecord {
            executable_hash: new_hash,
            connections,
            updated_at: now,
            path_hash: path_hash(path).or(previous.path_hash.clone()),
            known_slots: declared,
            ..previous
        });
        true
    }

    /// Drop the records of notebooks created here and never saved to a
    /// file (every record created-here with no path) whose newest record is
    /// older than [`ABANDONED_AFTER_DAYS`], unless `kept` says a recovery
    /// copy still holds the notebook. Returns how many notebooks were
    /// dropped.
    pub fn prune_abandoned(&mut self, now: DateTime<Utc>, kept: impl Fn(&str) -> bool) -> usize {
        let cutoff = now - chrono::Duration::days(ABANDONED_AFTER_DAYS);
        let before = self.notebooks.len();
        self.notebooks.retain(|id, records| {
            let abandoned = records.iter().all(|r| {
                r.origin == TrustOrigin::CreatedHere
                    && r.path_hash.is_none()
                    && r.updated_at < cutoff
            });
            !abandoned || kept(id)
        });
        before - self.notebooks.len()
    }

    /// Whether `notebook_id` belongs to a file at another path: this
    /// machine has records of it saved or trusted elsewhere and none at
    /// `path`. Such a file is a copy (copied by hand, or the original left
    /// behind by Save As) and must not share the other file's trust,
    /// history or local data.
    pub fn belongs_elsewhere(&self, notebook_id: &str, path: &std::path::Path) -> bool {
        let here = path_hash(Some(path));
        let mut elsewhere = false;
        for record in self.records(notebook_id) {
            match &record.path_hash {
                Some(hash) if Some(hash) == here.as_ref() => return false,
                Some(_) => elsewhere = true,
                None => {}
            }
        }
        elsewhere
    }

    /// Forget every trust record for a notebook.
    pub fn revoke(&mut self, notebook_id: &str) -> bool {
        self.notebooks.remove(notebook_id).is_some()
    }
}

/// The trust level of an opened document. Pure: reads the store only.
pub fn assess(
    store: &TrustStore,
    doc: &NotebookDoc,
    path: Option<&std::path::Path>,
) -> TrustDecision {
    let hash = executable_hash(doc);
    let records = store.records(&doc.notebook_id);
    let Some(record) = records
        .iter()
        .find(|r| r.executable_hash == hash && r.level != TrustLevel::Untrusted)
    else {
        let mut decision = TrustDecision::untrusted(doc);
        if !records.is_empty() {
            decision.reason = TrustReason::ChangedOutsideApp;
        }
        return decision;
    };
    let pending = declared_slots(doc)
        .into_iter()
        .filter(|slot| match record.connections.get(slot) {
            None => true,
            Some(profile) => slot != CURRENT_CONNECTION && *profile != profile_of(doc, slot),
        })
        .collect();
    let here = path_hash(path);
    TrustDecision {
        notebook_id: doc.notebook_id.clone(),
        level: record.level,
        reason: match record.origin {
            TrustOrigin::CreatedHere => TrustReason::CreatedHere,
            TrustOrigin::Granted => TrustReason::Granted,
        },
        executable_hash: hash,
        connections: record.connections.clone(),
        allow_writes: record.allow_writes && record.level == TrustLevel::Trusted,
        pending_connections: pending,
        moved: here.is_some() && record.path_hash.is_some() && here != record.path_hash,
        scope: Some(record.data_scope().to_string()),
    }
}

/// Record the user's trust choice for `doc` and return the new decision.
pub fn grant(
    store: &mut TrustStore,
    doc: &NotebookDoc,
    grant: &Grant,
    path: Option<&std::path::Path>,
    now: DateTime<Utc>,
) -> Result<TrustDecision, GrantError> {
    if grant.level == TrustLevel::Untrusted {
        return Err(GrantError::NotAGrant);
    }
    if doc.notebook_id.trim().is_empty() {
        return Err(GrantError::MissingNotebookId);
    }
    let slots = declared_slots(doc);
    let mut connections = BTreeMap::new();
    for slot in &grant.connections {
        if !slots.contains(slot) && slot != CURRENT_CONNECTION {
            return Err(GrantError::UnknownConnection { slot: slot.clone() });
        }
        connections.insert(slot.clone(), profile_of(doc, slot));
    }
    let scope = store.scope_for_grant(&doc.notebook_id, path);
    store.upsert(TrustRecord {
        notebook_id: doc.notebook_id.clone(),
        level: grant.level,
        executable_hash: executable_hash(doc),
        origin: TrustOrigin::Granted,
        connections,
        allow_writes: grant.allow_writes && grant.level == TrustLevel::Trusted,
        updated_at: now,
        path_hash: path_hash(path),
        known_slots: slots,
        scope,
    });
    Ok(assess(store, doc, path))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustStoreError {
    Io {
        detail: String,
    },
    /// The file exists but does not parse; callers treat every notebook as
    /// untrusted and must not overwrite it without telling the user.
    Corrupt {
        detail: String,
    },
    NewerVersion {
        found: u32,
    },
}

impl TrustStoreError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            TrustStoreError::Io { detail } => message(
                "notebook.security.trust_store.io",
                "Could not read or save notebook trust settings: {detail}",
                &[("detail", detail.clone())],
            ),
            TrustStoreError::Corrupt { .. } => message(
                "notebook.security.trust_store.corrupt",
                "Notebook trust settings are damaged, so every notebook is treated as untrusted until you trust it again.",
                &[],
            ),
            TrustStoreError::NewerVersion { .. } => message(
                "notebook.security.trust_store.newer",
                "Notebook trust settings were saved by a newer SQLLY, so every notebook is treated as untrusted here.",
                &[],
            ),
        }
    }
}

/// The trust file's name in the notebooks folder (and in browser storage).
pub const STORE_FILE: &str = "trust.json";

impl TrustStore {
    /// Parse a saved store.
    pub fn from_json(text: &str) -> Result<TrustStore, TrustStoreError> {
        let store: TrustStore =
            serde_json::from_str(text).map_err(|e| TrustStoreError::Corrupt {
                detail: e.to_string(),
            })?;
        if store.version > STORE_VERSION {
            return Err(TrustStoreError::NewerVersion {
                found: store.version,
            });
        }
        Ok(store)
    }

    /// The store as saved: pretty JSON with a trailing newline.
    pub fn to_json(&self) -> Result<String, TrustStoreError> {
        let mut text = serde_json::to_string_pretty(self).map_err(|e| TrustStoreError::Io {
            detail: e.to_string(),
        })?;
        text.push('\n');
        Ok(text)
    }

    /// Load the store kept under `name` in a text store (the browser
    /// build's storage); a missing entry is an empty store.
    pub fn load_from(
        store: &dyn crate::notebook::TextStore,
        name: &str,
    ) -> Result<TrustStore, TrustStoreError> {
        match store.read(name) {
            Ok(Some(text)) => Self::from_json(&text),
            Ok(None) => Ok(TrustStore::default()),
            Err(detail) => Err(TrustStoreError::Io { detail }),
        }
    }

    /// Save the store under `name` in a text store.
    pub fn save_to(
        &self,
        store: &dyn crate::notebook::TextStore,
        name: &str,
    ) -> Result<(), TrustStoreError> {
        store
            .write(name, &self.to_json()?)
            .map_err(|detail| TrustStoreError::Io { detail })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl TrustStore {
    /// `<data dir>/SQLLY/notebooks/trust.json`.
    pub fn default_path() -> std::path::PathBuf {
        crate::notebook::data_dir().join(STORE_FILE)
    }

    /// Load the store; a missing file is an empty store.
    pub fn load(path: &std::path::Path) -> Result<TrustStore, TrustStoreError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(TrustStore::default()),
            Err(e) => {
                return Err(TrustStoreError::Io {
                    detail: e.to_string(),
                });
            }
        };
        Self::from_json(&text)
    }

    /// Save atomically, owner-only.
    pub fn save(&self, path: &std::path::Path) -> Result<(), TrustStoreError> {
        let text = self.to_json()?;
        super::messages::private_fs::write_atomic(path, text.as_bytes()).map_err(|e| {
            TrustStoreError::Io {
                detail: e.to_string(),
            }
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::notebook::{CellKind, ConnectionRef, NotebookDoc, SeededIds};

    pub(crate) fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-18T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    pub(crate) fn sample() -> NotebookDoc {
        let mut ids = SeededIds(11);
        let mut doc = NotebookDoc::new("Revenue", &mut ids);
        doc.connection_refs.push(ConnectionRef {
            id: "warehouse".into(),
            profile_id: Some("p-1".into()),
            label: "Warehouse".into(),
            database: Some("sales".into()),
            extra: Default::default(),
        });
        doc.cells[0].source = "SELECT 1".into();
        doc.cells[0].connection_ref = Some("warehouse".into());
        let i = doc.add_cell(CellKind::Markdown, None, &mut ids);
        doc.cells[i].source = "# Notes".into();
        doc
    }

    #[test]
    fn bookmarks_folds_and_pins_do_not_change_the_executable_hash() {
        let doc = sample();
        let mut marked = doc.clone();
        for key in crate::notebook::editing::VIEW_KEYS {
            marked.cells[0]
                .extra
                .insert(key.to_string(), serde_json::Value::Bool(true));
        }
        assert_eq!(executable_hash(&doc), executable_hash(&marked));
        marked.cells[0]
            .extra
            .insert("other".into(), serde_json::Value::Bool(true));
        assert_ne!(executable_hash(&doc), executable_hash(&marked));
    }

    #[test]
    fn display_settings_do_not_change_the_executable_hash() {
        use crate::notebook::display;
        let doc = sample();
        let mut shown = doc.clone();
        display::set_all(&mut shown, display::DisplayMode::Embedded);
        display::update(&mut shown.cells[0], |d| {
            d.mode = Some(display::DisplayMode::Full);
            d.config_shown = Some(true);
            d.height = Some(320.0);
            d.lock = Some(display::LockedView::Pivot);
        });
        assert_eq!(executable_hash(&doc), executable_hash(&shown));
        shown.settings.default_policy.read_only = !shown.settings.default_policy.read_only;
        assert_ne!(executable_hash(&doc), executable_hash(&shown));
    }

    #[test]
    fn a_file_reusing_a_notebook_id_gets_its_own_data_scope() {
        let doc = sample();
        let mut store = TrustStore::default();
        let home = std::path::Path::new("/work/revenue.sqllynb");
        let created = store.record_created(&doc, None, now());
        assert_eq!(created.scope.as_deref(), Some(doc.notebook_id.as_str()));
        assert!(store.record_in_app_save(&created.executable_hash, &doc, Some(home), now()));
        assert_eq!(
            assess(&store, &doc, Some(home)).scope.as_deref(),
            Some(doc.notebook_id.as_str())
        );
        assert_eq!(assess(&TrustStore::default(), &doc, None).scope, None);

        // A hostile copy elsewhere with the same id and different SQL.
        let mut forged = doc.clone();
        forged.cells[0].source = "SELECT * FROM {{secrets}}".into();
        let elsewhere = std::path::Path::new("/tmp/evil.sqllynb");
        assert_eq!(assess(&store, &forged, Some(elsewhere)).scope, None);
        let all = Grant {
            level: TrustLevel::Trusted,
            connections: declared_slots(&forged),
            allow_writes: false,
        };
        let granted = grant(&mut store, &forged, &all, Some(elsewhere), now()).unwrap();
        let forged_scope = granted.scope.unwrap();
        assert_ne!(forged_scope, doc.notebook_id, "it inherits nothing");
        assert!(forged_scope.starts_with("p_"));
        assert_eq!(
            grant(&mut store, &forged, &all, Some(elsewhere), now())
                .unwrap()
                .scope
                .as_deref(),
            Some(forged_scope.as_str()),
            "the same file trusted again keeps its scope"
        );
        let unsaved = grant(&mut TrustStore::default(), &forged, &all, None, now()).unwrap();
        assert!(unsaved.scope.unwrap().starts_with("s_"));

        // The real file edited outside the app and trusted again keeps its data.
        let mut edited = doc.clone();
        edited.cells[0].source = "SELECT 2".into();
        let again = grant(&mut store, &edited, &all, Some(home), now()).unwrap();
        assert_eq!(again.scope.as_deref(), Some(doc.notebook_id.as_str()));

        // Records written before scopes existed keep the notebook id.
        let legacy: TrustRecord = serde_json::from_value(json!({
            "notebook_id": "nb_old", "level": "trusted", "executable_hash": "h",
            "origin": "granted", "connections": {}, "allow_writes": false,
            "updated_at": "2026-11-01T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(legacy.data_scope(), "nb_old");
    }

    #[test]
    fn unknown_notebooks_are_untrusted_and_list_their_connections() {
        let doc = sample();
        let decision = assess(&TrustStore::default(), &doc, None);
        assert_eq!(decision.level, TrustLevel::Untrusted);
        assert_eq!(decision.reason, TrustReason::NotTrustedHere);
        assert_eq!(decision.pending_connections, vec!["warehouse".to_string()]);
        assert!(decision.needs_banner());
        assert!(!decision.authorizes("warehouse", Some("p-1")));
    }

    #[test]
    fn created_notebooks_are_trusted_with_every_slot() {
        let doc = sample();
        let mut store = TrustStore::default();
        let decision = store.record_created(&doc, None, now());
        assert_eq!(decision.level, TrustLevel::Trusted);
        assert_eq!(decision.reason, TrustReason::CreatedHere);
        assert!(decision.allow_writes);
        assert!(decision.pending_connections.is_empty());
        assert!(decision.authorizes("warehouse", Some("p-1")));
        assert!(!decision.authorizes("warehouse", Some("p-2")));
    }

    #[test]
    fn markdown_and_title_edits_keep_trust_but_sql_edits_outside_drop_it() {
        let doc = sample();
        let mut store = TrustStore::default();
        store.record_created(&doc, None, now());

        let mut prose = doc.clone();
        prose.title = "Renamed".into();
        prose.cells[1].source = "# Different notes".into();
        prose.cells[0].collapsed = true;
        assert_eq!(assess(&store, &prose, None).level, TrustLevel::Trusted);

        for mutate in [
            |d: &mut NotebookDoc| d.cells[0].source = "DROP TABLE t".into(),
            |d: &mut NotebookDoc| d.cells[0].execution_policy.read_only = false,
            |d: &mut NotebookDoc| d.connection_refs[0].profile_id = Some("p-evil".into()),
            |d: &mut NotebookDoc| d.connection_refs[0].database = Some("master".into()),
            |d: &mut NotebookDoc| d.cells[0].session_group = Some("g".into()),
            |d: &mut NotebookDoc| d.settings.run_mode = crate::notebook::RunMode::Reactive,
            |d: &mut NotebookDoc| d.cells[1].kind = CellKind::Sql,
            |d: &mut NotebookDoc| d.cells.swap(0, 1),
            |d: &mut NotebookDoc| {
                d.extra.insert("future_autorun".into(), json!(true));
            },
        ] {
            let mut changed = doc.clone();
            mutate(&mut changed);
            let decision = assess(&store, &changed, None);
            assert_eq!(decision.level, TrustLevel::Untrusted);
            assert_eq!(decision.reason, TrustReason::ChangedOutsideApp);
        }
    }

    #[test]
    fn hash_ignores_json_field_order() {
        let doc = sample();
        let mut value: Value = serde_json::from_str(&doc.to_canonical_json()).unwrap();
        let obj = value.as_object_mut().unwrap();
        let reordered: serde_json::Map<String, Value> = obj
            .iter()
            .rev()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let text = serde_json::to_string(&Value::Object(reordered)).unwrap();
        let loaded: NotebookDoc = serde_json::from_str(&text).unwrap();
        assert_eq!(executable_hash(&loaded), executable_hash(&doc));
    }

    #[test]
    fn grants_record_connections_and_remaps_need_a_new_grant() {
        let doc = sample();
        let mut store = TrustStore::default();
        let decision = grant(
            &mut store,
            &doc,
            &Grant {
                level: TrustLevel::Restricted,
                connections: ["warehouse".to_string()].into(),
                allow_writes: true,
            },
            None,
            now(),
        )
        .unwrap();
        assert_eq!(decision.level, TrustLevel::Restricted);
        assert!(!decision.allow_writes, "restricted never allows writes");
        assert_eq!(decision.connections["warehouse"], Some("p-1".into()));

        let mut remapped = doc.clone();
        remapped.connection_refs[0].profile_id = Some("p-2".into());
        assert_eq!(assess(&store, &remapped, None).level, TrustLevel::Untrusted);
    }

    #[test]
    fn grant_rejects_unknown_slots_and_untrusted_level() {
        let doc = sample();
        let mut store = TrustStore::default();
        let bad = Grant {
            level: TrustLevel::Trusted,
            connections: ["elsewhere".to_string()].into(),
            allow_writes: false,
        };
        assert_eq!(
            grant(&mut store, &doc, &bad, None, now()),
            Err(GrantError::UnknownConnection {
                slot: "elsewhere".into()
            })
        );
        let none = Grant {
            level: TrustLevel::Untrusted,
            connections: BTreeSet::new(),
            allow_writes: false,
        };
        assert_eq!(
            grant(&mut store, &doc, &none, None, now()),
            Err(GrantError::NotAGrant)
        );
        assert!(store.notebooks.is_empty());
    }

    #[test]
    fn partial_grants_leave_other_slots_pending() {
        let mut doc = sample();
        doc.connection_refs.push(ConnectionRef {
            id: "crm".into(),
            profile_id: Some("p-9".into()),
            label: "CRM".into(),
            database: None,
            extra: Default::default(),
        });
        let mut store = TrustStore::default();
        let decision = grant(
            &mut store,
            &doc,
            &Grant {
                level: TrustLevel::Trusted,
                connections: ["warehouse".to_string()].into(),
                allow_writes: false,
            },
            None,
            now(),
        )
        .unwrap();
        assert_eq!(decision.pending_connections, vec!["crm".to_string()]);
        assert!(!decision.authorizes("crm", Some("p-9")));
    }

    #[test]
    fn in_app_saves_follow_the_hash_only_from_a_trusted_state() {
        let doc = sample();
        let mut store = TrustStore::default();
        let before = store.record_created(&doc, None, now()).executable_hash;
        let mut edited = doc.clone();
        edited.cells[0].source = "SELECT 2".into();
        assert!(store.record_in_app_save(&before, &edited, None, now()));
        assert_eq!(assess(&store, &edited, None).level, TrustLevel::Trusted);
        assert_eq!(assess(&store, &doc, None).level, TrustLevel::Untrusted);

        let stranger = {
            let mut d = sample();
            d.notebook_id = "someone-else".into();
            d
        };
        let hash = executable_hash(&stranger);
        assert!(!store.record_in_app_save(&hash, &stranger, None, now()));
        assert_eq!(assess(&store, &stranger, None).level, TrustLevel::Untrusted);
    }

    #[test]
    fn in_app_saves_keep_left_out_slots_pending_and_authorize_added_ones() {
        let mut doc = sample();
        doc.connection_refs.push(ConnectionRef {
            id: "crm".into(),
            profile_id: Some("p-9".into()),
            label: "CRM".into(),
            database: None,
            extra: Default::default(),
        });
        let mut store = TrustStore::default();
        let before = grant(
            &mut store,
            &doc,
            &Grant {
                level: TrustLevel::Restricted,
                connections: ["warehouse".to_string()].into(),
                allow_writes: false,
            },
            None,
            now(),
        )
        .unwrap()
        .executable_hash;
        let mut edited = doc.clone();
        edited.cells[0].source = "SELECT 2".into();
        edited.connection_refs[0].profile_id = Some("p-remapped".into());
        edited.connection_refs.push(ConnectionRef {
            id: "added".into(),
            profile_id: Some("p-5".into()),
            label: "Added here".into(),
            database: None,
            extra: Default::default(),
        });
        assert!(store.record_in_app_save(&before, &edited, None, now()));
        let decision = assess(&store, &edited, None);
        assert_eq!(decision.level, TrustLevel::Restricted);
        assert_eq!(decision.pending_connections, vec!["crm".to_string()]);
        assert!(decision.authorizes("warehouse", Some("p-remapped")));
        assert!(decision.authorizes("added", Some("p-5")));
        assert!(!decision.authorizes("crm", Some("p-9")));
    }

    #[test]
    fn moved_copies_are_noted_but_stay_trusted() {
        let doc = sample();
        let mut store = TrustStore::default();
        store.record_created(&doc, Some(std::path::Path::new("a.sqllynb")), now());
        let decision = assess(&store, &doc, Some(std::path::Path::new("b.sqllynb")));
        assert_eq!(decision.level, TrustLevel::Trusted);
        assert!(decision.moved);
    }

    #[test]
    fn notebooks_created_here_and_never_saved_are_pruned_after_a_while() {
        let old = now() - chrono::Duration::days(ABANDONED_AFTER_DAYS + 1);
        let mut ids = SeededIds(90);
        let mut store = TrustStore::default();
        let abandoned = NotebookDoc::new("Scratch", &mut ids);
        store.record_created(&abandoned, None, old);
        let recovered = NotebookDoc::new("Kept by a recovery copy", &mut ids);
        store.record_created(&recovered, None, old);
        let saved = NotebookDoc::new("Saved", &mut ids);
        store.record_created(&saved, Some(std::path::Path::new("saved.sqllynb")), old);
        let recent = NotebookDoc::new("Recent", &mut ids);
        store.record_created(&recent, None, now() - chrono::Duration::days(2));
        let granted = sample();
        grant(
            &mut store,
            &granted,
            &Grant {
                level: TrustLevel::Trusted,
                connections: declared_slots(&granted),
                allow_writes: false,
            },
            None,
            old,
        )
        .unwrap();

        let dropped = store.prune_abandoned(now(), |id| id == recovered.notebook_id);

        assert_eq!(dropped, 1);
        assert!(!store.notebooks.contains_key(&abandoned.notebook_id));
        for kept in [&recovered, &saved, &recent, &granted] {
            assert!(
                store.notebooks.contains_key(&kept.notebook_id),
                "{}",
                kept.title
            );
        }
    }

    #[test]
    fn a_file_whose_id_was_saved_elsewhere_is_a_copy() {
        let doc = sample();
        let original = std::path::Path::new("/work/report.sqllynb");
        let copy = std::path::Path::new("/work/report copy.sqllynb");
        let mut store = TrustStore::default();
        assert!(
            !store.belongs_elsewhere(&doc.notebook_id, copy),
            "unknown ids are no copy"
        );
        store.record_created(&doc, None, now());
        assert!(
            !store.belongs_elsewhere(&doc.notebook_id, copy),
            "a never-saved record names no file"
        );
        store.record_created(&doc, Some(original), now());
        assert!(store.belongs_elsewhere(&doc.notebook_id, copy));
        assert!(!store.belongs_elsewhere(&doc.notebook_id, original));
    }

    #[test]
    fn records_per_notebook_are_capped() {
        let mut doc = sample();
        let mut store = TrustStore::default();
        for i in 0..(MAX_RECORDS_PER_NOTEBOOK + 3) {
            doc.cells[0].source = format!("SELECT {i}");
            store.record_created(&doc, None, now() + chrono::Duration::seconds(i as i64));
        }
        assert_eq!(
            store.notebooks[&doc.notebook_id].len(),
            MAX_RECORDS_PER_NOTEBOOK
        );
        assert_eq!(assess(&store, &doc, None).level, TrustLevel::Trusted);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn store_file_round_trips_owner_only_and_corruption_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("trust.json");
        assert_eq!(TrustStore::load(&path).unwrap(), TrustStore::default());
        let mut store = TrustStore::default();
        store.record_created(&sample(), None, now());
        store.save(&path).unwrap();
        assert_eq!(TrustStore::load(&path).unwrap(), store);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::write(&path, "{ not json").unwrap();
        assert!(matches!(
            TrustStore::load(&path),
            Err(TrustStoreError::Corrupt { .. })
        ));
        std::fs::write(&path, r#"{"version": 99, "notebooks": {}}"#).unwrap();
        assert_eq!(
            TrustStore::load(&path),
            Err(TrustStoreError::NewerVersion { found: 99 })
        );
    }

    #[test]
    fn a_text_store_keeps_the_same_trust_file() {
        use crate::notebook::{MemoryTextStore, TextStore};
        let storage = MemoryTextStore::default();
        assert_eq!(
            TrustStore::load_from(&storage, STORE_FILE).unwrap(),
            TrustStore::default()
        );
        let mut store = TrustStore::default();
        store.record_created(&sample(), None, now());
        store.save_to(&storage, STORE_FILE).unwrap();
        let text = storage.read(STORE_FILE).unwrap().unwrap();
        assert_eq!(text, store.to_json().unwrap());
        assert!(text.ends_with("}\n"));
        assert_eq!(TrustStore::load_from(&storage, STORE_FILE).unwrap(), store);
        storage.write(STORE_FILE, "{ not json").unwrap();
        assert!(matches!(
            TrustStore::load_from(&storage, STORE_FILE),
            Err(TrustStoreError::Corrupt { .. })
        ));
        storage
            .write(STORE_FILE, r#"{"version": 99, "notebooks": {}}"#)
            .unwrap();
        assert_eq!(
            TrustStore::load_from(&storage, STORE_FILE),
            Err(TrustStoreError::NewerVersion { found: 99 })
        );
        assert!(matches!(
            store.save_to(&storage, "../trust.json"),
            Err(TrustStoreError::Io { .. })
        ));
    }
}
