//! Notebook trust on this machine, kept by
//! `sqlly_services::notebook_security::trust`: a notebook runs only when it
//! was created here or the user trusted it, and only while its queries and
//! connections hash to what was trusted. A change made outside the app (a
//! `git pull`, a sync folder) opens it untrusted again. Opening a notebook
//! never runs a cell either way, and every run is written to the notebook
//! audit log without its SQL.
//!
//! The view keeps the decision it opened (or last saved) with as its base;
//! [`live`] applies the user's in-app edits on top of it, so a slot the user
//! added runs at once while a slot remapped to another profile waits for a
//! new grant.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::Utc;
use gpui::Context;
use sqlly_services::notebook::NotebookDoc;
use sqlly_services::notebook_security::audit::{
    AuditAction, AuditEvent, AuditOutcome, AuditPrivacy,
};
use sqlly_services::notebook_security::trust::{
    self, Grant, TrustDecision, TrustLevel, TrustOrigin, TrustReason, TrustRecord, TrustStore,
};
use sqlly_services::notebook_security::{
    AuthContext, ConnectionFacts, Consents, DataPolicy, Decision, NotebookAction, Reason, authorize,
};

use super::connection::ConnectionChoice;
use super::view::NotebookView;

pub use sqlly_services::notebook_security::trust::CURRENT_CONNECTION;

thread_local! {
    /// Trust recorded while the trust file can't be read: it lasts until the
    /// app quits, and the file is left as it is for the user to deal with.
    static SESSION: RefCell<TrustStore> = RefCell::new(TrustStore::default());
}

/// A trust decision, and what to tell the user when the trust file could
/// not be read or written.
#[derive(Debug, Clone)]
pub struct Assessed {
    pub decision: TrustDecision,
    pub problem: Option<String>,
}

fn text(message: sqlly_core::DisplayMessage) -> String {
    crate::localization::message(message)
}

fn store_path() -> PathBuf {
    super::autosave::notebooks_dir().join("trust.json")
}

fn audit_dir() -> PathBuf {
    super::autosave::notebooks_dir().join("audit")
}

/// Run `f` on this machine's trust store, saving it afterwards when `write`.
/// An unreadable (damaged, newer, inaccessible) file is never overwritten:
/// `f` runs on the session store instead and the problem is returned.
#[cfg(not(target_arch = "wasm32"))]
fn with_store<R>(write: bool, f: impl FnOnce(&mut TrustStore) -> R) -> (R, Option<String>) {
    let path = store_path();
    match TrustStore::load(&path) {
        Ok(mut store) => {
            let out = f(&mut store);
            let problem = write
                .then(|| store.save(&path).err().map(|e| text(e.message())))
                .flatten();
            (out, problem)
        }
        Err(error) => (
            SESSION.with(|s| f(&mut s.borrow_mut())),
            Some(text(error.message())),
        ),
    }
}

/// Browser build: the same trust file, kept in the page's notebooks folder.
/// There is no machine to tie it to, so trust is per browser profile.
#[cfg(target_arch = "wasm32")]
fn with_store<R>(write: bool, f: impl FnOnce(&mut TrustStore) -> R) -> (R, Option<String>) {
    let _ = store_path;
    with_text_store(super::browser::store(), write, f)
}

/// [`with_store`] over a text store: same rules, an unreadable trust entry
/// is left alone and the session store used instead.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn with_text_store<R>(
    storage: &dyn sqlly_services::notebook::TextStore,
    write: bool,
    f: impl FnOnce(&mut TrustStore) -> R,
) -> (R, Option<String>) {
    use sqlly_services::notebook_security::trust::STORE_FILE;
    match TrustStore::load_from(storage, STORE_FILE) {
        Ok(mut store) => {
            let out = f(&mut store);
            let problem = write
                .then(|| {
                    store
                        .save_to(storage, STORE_FILE)
                        .err()
                        .map(|e| text(e.message()))
                })
                .flatten();
            (out, problem)
        }
        Err(error) => (
            SESSION.with(|s| f(&mut s.borrow_mut())),
            Some(text(error.message())),
        ),
    }
}

/// The trust of a notebook just opened from a file, recovery copy or session.
pub fn assess(doc: &NotebookDoc, path: Option<&Path>) -> Assessed {
    let (decision, problem) = with_store(false, |store| trust::assess(store, doc, path));
    Assessed { decision, problem }
}

/// A notebook the user just created in the app: trusted, every slot. Old
/// records of notebooks created here, never saved and with no recovery copy
/// are dropped at the same time.
pub fn record_created(doc: &NotebookDoc, path: Option<&Path>) -> Assessed {
    let (decision, problem) = with_store(true, |store| {
        let now = Utc::now();
        store.prune_abandoned(now, super::autosave::has_recovery);
        store.record_created(doc, path, now)
    });
    Assessed { decision, problem }
}

/// Whether the file at `path` is a copy of a notebook this machine knows at
/// another path.
pub fn belongs_elsewhere(doc: &NotebookDoc, path: &Path) -> bool {
    with_store(false, |store| {
        store.belongs_elsewhere(&doc.notebook_id, path)
    })
    .0
}

/// The user's choice in the trust dialog.
pub fn grant(doc: &NotebookDoc, choice: &Grant, path: Option<&Path>) -> Result<Assessed, String> {
    let (result, problem) = with_store(true, |store| {
        trust::grant(store, doc, choice, path, Utc::now())
    });
    result
        .map(|decision| Assessed { decision, problem })
        .map_err(|e| text(e.message()))
}

fn profile_of(doc: &NotebookDoc, slot: &str) -> Option<String> {
    doc.connection(slot).and_then(|c| c.profile_id.clone())
}

/// `base` with the in-app edits made to `doc` since: slots the user added
/// are authorized for the profile they picked, a slot remapped to another
/// profile is pending until the user trusts the notebook again, and slots
/// the grant left out stay pending.
pub fn live(base: &TrustDecision, doc: &NotebookDoc) -> TrustDecision {
    let declared = trust::declared_slots(doc);
    if base.level == TrustLevel::Untrusted {
        return TrustDecision {
            pending_connections: declared.into_iter().collect(),
            ..base.clone()
        };
    }
    let known = |slot: &str| {
        base.connections.contains_key(slot) || base.pending_connections.iter().any(|s| s == slot)
    };
    let mut connections = BTreeMap::new();
    let mut pending = Vec::new();
    for slot in declared {
        let profile = profile_of(doc, &slot);
        let authorized = match base.connections.get(&slot) {
            Some(granted) => slot == CURRENT_CONNECTION || *granted == profile,
            None => !known(&slot),
        };
        if authorized {
            connections.insert(slot, profile);
        } else {
            pending.push(slot);
        }
    }
    TrustDecision {
        connections,
        pending_connections: pending,
        ..base.clone()
    }
}

/// The user saved `doc` in the app (Save, Save As, autosave, recovery copy).
/// A trusted notebook's record moves to the saved contents, keeping exactly
/// what [`live`] authorizes; an untrusted one is only re-assessed.
pub fn record_save(base: &TrustDecision, doc: &NotebookDoc, path: Option<&Path>) -> Assessed {
    if base.level == TrustLevel::Untrusted || doc.notebook_id.trim().is_empty() {
        let mut assessed = assess(doc, path);
        if assessed.decision.level == TrustLevel::Untrusted {
            assessed.decision.reason = base.reason;
        }
        return assessed;
    }
    let live = live(base, doc);
    let now = Utc::now();
    let hash = trust::executable_hash(doc);
    let (decision, problem) = with_store(true, |store| {
        if !store.record_in_app_save(&base.executable_hash, doc, path, now) {
            // Another tab saved the same notebook first, or its record aged
            // out: what this tab trusts is still the user's own work.
            let record = TrustRecord {
                notebook_id: doc.notebook_id.clone(),
                level: base.level,
                executable_hash: hash.clone(),
                origin: match base.reason {
                    TrustReason::CreatedHere => TrustOrigin::CreatedHere,
                    _ => TrustOrigin::Granted,
                },
                connections: BTreeMap::new(),
                allow_writes: base.allow_writes,
                updated_at: now,
                path_hash: None,
                known_slots: trust::declared_slots(doc),
                scope: base.scope.clone().unwrap_or_default(),
            };
            let list = store.notebooks.entry(doc.notebook_id.clone()).or_default();
            list.retain(|r| r.executable_hash != hash);
            list.push(record);
        }
        // In-app remaps still need a new grant.
        if let Some(record) = store
            .notebooks
            .get_mut(&doc.notebook_id)
            .and_then(|list| list.iter_mut().find(|r| r.executable_hash == hash))
        {
            record.connections = live.connections.clone();
        }
        trust::assess(store, doc, path)
    });
    Assessed { decision, problem }
}

/// Who ran a cell, as the audit log names them.
pub fn actor() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// Append `event` to the notebook audit log. A failed write is logged, never
/// shown: it must not stop the query the user asked for.
pub fn audit(event: &AuditEvent) {
    #[cfg(not(target_arch = "wasm32"))]
    if let Err(error) =
        sqlly_services::notebook_security::audit::AuditLog::new(audit_dir()).append(event)
    {
        tracing::warn!(%error, "notebook audit log write failed");
    }
    #[cfg(target_arch = "wasm32")]
    let _ = (event, audit_dir);
}

/// What the trust dialog has ticked so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantDraft {
    pub connections: BTreeSet<String>,
    /// [`TrustLevel::Trusted`] or [`TrustLevel::Restricted`] (read-only trust).
    pub level: TrustLevel,
    pub allow_writes: bool,
    pub error: Option<String>,
}

fn connection_facts(choice: Option<&ConnectionChoice>) -> ConnectionFacts {
    ConnectionFacts {
        profile_id: choice.map(|c| c.profile_id.clone()),
        engine: choice.map(|c| c.database_type),
        environment: choice.map(|c| c.environment).unwrap_or_default(),
        allow_local_materialize: false,
        // The production guard decides when the query runs.
        writes_unlocked: true,
    }
}

impl NotebookView {
    /// The trust that applies right now: the base decision plus in-app edits.
    pub(super) fn live_trust(&self) -> TrustDecision {
        let generation = self.doc.generation();
        if let Some((at, base, live)) = self.trust_live.borrow().as_ref()
            && *at == generation
            && *base == self.trust
        {
            return live.clone();
        }
        #[cfg(test)]
        super::perf_counters::bump(super::perf_counters::Pass::Trust);
        let decision = live(&self.trust, self.doc.model());
        *self.trust_live.borrow_mut() = Some((generation, self.trust.clone(), decision.clone()));
        decision
    }

    pub(super) fn set_trust(&mut self, assessed: Assessed) {
        self.trust = assessed.decision;
        self.note_trust_problem(assessed.problem);
    }

    pub(super) fn note_trust_problem(&mut self, problem: Option<String>) {
        if let Some(message) = &problem
            && self.trust_problem.as_ref() != Some(message)
        {
            self.log(None, message.clone(), true, None);
        }
        self.trust_problem = problem;
    }

    /// Re-assess once the file the notebook came from is known.
    pub(super) fn reassess_trust_at(&mut self, path: &str) {
        self.copy_pending = belongs_elsewhere(self.doc.model(), Path::new(path));
        self.set_trust(assess(self.doc.model(), Some(Path::new(path))));
        if self.trust.moved {
            self.log(
                None,
                crate::localization::text("notebook.shell.trust.moved"),
                false,
                None,
            );
        }
    }

    /// A copy of another notebook (a copied file, or the original left
    /// behind by Save As) takes a new id before it is first saved, so it
    /// never shares that notebook's trust record, run history or local
    /// data; the other file keeps them. The user is told once.
    pub(crate) fn take_own_identity(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.copy_pending) {
            return;
        }
        let id = sqlly_services::notebook::document::new_notebook_id(
            &mut sqlly_services::notebook::RandomIds,
        );
        let path = self.file_path.clone();
        let (scope, _) = with_store(false, |store| {
            store.scope_for_grant(&id, path.as_deref().map(Path::new))
        });
        self.doc.set_id(id);
        if self.trust.level != TrustLevel::Untrusted {
            self.trust.scope = Some(scope);
        }
        self.reopen_history();
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.local.workspace = None;
        }
        let message = crate::localization::text("notebook.fu_core.copy.own_identity");
        self.log(None, message.clone(), false, None);
        self.status(message, false, cx);
    }

    /// The notebook was written to `path` (or its recovery copy).
    pub(super) fn record_trusted_save(&mut self, path: Option<&str>) {
        let assessed = record_save(&self.trust, self.doc.model(), path.map(Path::new));
        self.set_trust(assessed);
    }

    /// A slot's name as the trust banner and dialog show it.
    pub(crate) fn slot_label(&self, slot: &str) -> String {
        if slot == CURRENT_CONNECTION {
            return crate::localization::text("notebook.shell.trust.current_connection");
        }
        self.doc
            .model()
            .connection(slot)
            .map(|c| c.label.clone())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| slot.to_string())
    }

    /// The local profile `slot` runs on, if this machine has it.
    pub(crate) fn slot_choice(&self, slot: &str) -> Option<ConnectionChoice> {
        if slot == CURRENT_CONNECTION {
            return self.tab_connection.clone();
        }
        let profile = profile_of(self.doc.model(), slot)?;
        self.choices
            .iter()
            .find(|c| c.profile_id == profile)
            .cloned()
    }

    /// `notebook_security::authorize` for running `id`, from what this view
    /// knows: the live trust, each slot's local profile and environment,
    /// and each cell's own Allow writes (part of what was trusted).
    pub(super) fn run_decision(&self, id: &str) -> Decision {
        self.decide(&NotebookAction::RunCell { cell_id: id })
    }

    /// `authorize(action)` with the notebook's live trust, its connections
    /// as this machine maps them, and its write cells elevated (the
    /// mutation review still runs on their SQL).
    pub(super) fn decide(&self, action: &NotebookAction<'_>) -> Decision {
        let doc = self.doc.model();
        let trust = self.live_trust();
        let mut connections: BTreeMap<String, ConnectionFacts> = doc
            .connection_refs
            .iter()
            .map(|slot| {
                (
                    slot.id.clone(),
                    connection_facts(self.slot_choice(&slot.id).as_ref()),
                )
            })
            .collect();
        connections.insert(
            CURRENT_CONNECTION.to_string(),
            connection_facts(self.tab_connection.as_ref()),
        );
        let consents = Consents {
            elevated_cells: doc
                .cells
                .iter()
                .filter(|c| !c.execution_policy.read_only)
                .map(|c| c.id.clone())
                .collect(),
            ..Consents::default()
        };
        authorize(
            action,
            &AuthContext {
                doc,
                trust: &trust,
                connections: &connections,
                policy: &DataPolicy::default(),
                consents: &consents,
            },
        )
    }

    /// Why trust stops a run, if it does. Unmapped connections are left to
    /// the remap prompt and the host's own no-connection error.
    pub(super) fn run_refusal(&self, decision: &Decision) -> Option<sqlly_core::DisplayMessage> {
        let reason = match decision {
            Decision::Allow { .. } | Decision::Deny(Reason::ConnectionUnmapped { .. }) => {
                return None;
            }
            Decision::Deny(reason) | Decision::NeedsApproval(reason) => reason,
        };
        Some(match reason {
            Reason::TrustRequired => {
                crate::localization::deferred_text("notebook.shell.trust.run_blocked")
            }
            Reason::ConnectionNotAuthorized { slot } => crate::localization::deferred(
                "notebook.shell.trust.connection_not_allowed",
                &[("connection_name", self.slot_label(slot))],
            ),
            other => other.message(),
        })
    }

    fn audit_event(
        &self,
        cell: Option<&str>,
        action: AuditAction,
        decision: &Decision,
    ) -> AuditEvent {
        let doc = self.doc.model();
        let event = AuditEvent::new(
            Utc::now(),
            actor(),
            doc.notebook_id.as_str(),
            trust::executable_hash(doc),
            action,
            decision,
        );
        let Some(id) = cell else {
            return event;
        };
        let (choice, _) = self.cell_connection(id);
        event.with_cell(id).with_connection(
            choice.as_ref().map(|c| c.profile_id.as_str()),
            choice.as_ref().map(|c| c.environment).unwrap_or_default(),
        )
    }

    /// The check right before `id` is handed to the host. A refusal is
    /// audited and explained in the trust banner; an allowed run is audited
    /// with its query hash (never its SQL) and finished by [`Self::finish_audit`].
    pub(super) fn check_run(&mut self, id: &str) -> Result<(), sqlly_core::DisplayMessage> {
        let decision = self.run_decision(id);
        let event = self.audit_event(Some(id), AuditAction::RunCell, &decision);
        if let Some(message) = self.run_refusal(&decision) {
            audit(&event);
            self.trust_flash = true;
            self.trust_notice = Some(crate::localization::message(&message));
            return Err(message);
        }
        if decision.is_allowed() {
            let event = match self.run_request(id) {
                Ok(request) => event.with_sql(
                    request.history_sql.as_deref().unwrap_or(&request.sql),
                    &AuditPrivacy::default(),
                ),
                Err(_) => event,
            };
            audit(&event);
            self.cells.entry(id.to_string()).or_default().audit = Some(event);
        }
        Ok(())
    }

    /// Close the audit event of `id`'s run in flight, if any.
    pub(super) fn finish_audit(
        &mut self,
        id: &str,
        outcome: AuditOutcome,
        counts: Option<(u64, Option<u64>)>,
        error: Option<&str>,
    ) {
        let Some(mut event) = self.cells.get_mut(id).and_then(|rt| rt.audit.take()) else {
            return;
        };
        event.at = Utc::now();
        if let Some((rows, elapsed_ms)) = counts {
            event = event.with_counts(Some(rows), None, elapsed_ms);
        }
        audit(&event.with_outcome(outcome, error));
    }

    /// A run asked of a notebook nobody trusted here yet: nothing runs.
    pub(super) fn refuse_untrusted_run(&mut self, anchor: Option<&str>) {
        self.trust_flash = true;
        let decision = Decision::NeedsApproval(Reason::TrustRequired);
        audit(&self.audit_event(anchor, AuditAction::RunCell, &decision));
    }

    /// Open the trust dialog: every connection ticked, writes as trusted
    /// before (off for a notebook from elsewhere).
    pub(crate) fn trust_notebook(&mut self, cx: &mut Context<Self>) {
        let live = self.live_trust();
        self.grant = Some(GrantDraft {
            connections: trust::declared_slots(self.doc.model()),
            level: if live.level == TrustLevel::Restricted {
                TrustLevel::Restricted
            } else {
                TrustLevel::Trusted
            },
            allow_writes: live.level != TrustLevel::Untrusted && live.allow_writes,
            error: None,
        });
        cx.notify();
    }

    /// The connection slots the trust dialog lists, in the notebook's order.
    pub(crate) fn grant_slots(&self) -> Vec<String> {
        trust::declared_slots(self.doc.model())
            .into_iter()
            .collect()
    }

    pub(crate) fn set_grant_connection(&mut self, slot: &str, on: bool, cx: &mut Context<Self>) {
        if let Some(draft) = self.grant.as_mut() {
            if on {
                draft.connections.insert(slot.to_string());
            } else {
                draft.connections.remove(slot);
            }
            cx.notify();
        }
    }

    pub(crate) fn set_grant_level(&mut self, level: TrustLevel, cx: &mut Context<Self>) {
        if let Some(draft) = self.grant.as_mut() {
            draft.level = level;
            cx.notify();
        }
    }

    pub(crate) fn set_grant_allow_writes(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(draft) = self.grant.as_mut() {
            draft.allow_writes = on;
            cx.notify();
        }
    }

    pub(crate) fn cancel_grant(&mut self, cx: &mut Context<Self>) {
        self.grant = None;
        cx.notify();
    }

    /// Record the dialog's choice and close it.
    pub(crate) fn confirm_grant(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.grant.clone() else {
            return;
        };
        let choice = Grant {
            level: draft.level,
            connections: draft.connections,
            allow_writes: draft.allow_writes && draft.level == TrustLevel::Trusted,
        };
        let path = self.file_path.clone();
        match grant(self.doc.model(), &choice, path.as_deref().map(Path::new)) {
            Ok(assessed) => {
                self.grant = None;
                self.trust_flash = false;
                self.trust_notice = None;
                self.set_trust(assessed);
                let decision = Decision::Allow { notices: vec![] };
                audit(&self.audit_event(None, AuditAction::TrustGranted, &decision));
                let key = if choice.level == TrustLevel::Restricted {
                    "notebook.shell.trust.activity_read_only"
                } else {
                    "notebook.shell.activity.trusted"
                };
                self.log(None, crate::localization::text(key), false, None);
                self.restore_cached_outputs(cx);
            }
            Err(message) => {
                if let Some(draft) = self.grant.as_mut() {
                    draft.error = Some(message);
                }
            }
        }
        cx.notify();
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(super) fn recent_audit(limit: usize) -> Vec<AuditEvent> {
    sqlly_services::notebook_security::audit::AuditLog::new(audit_dir()).recent(limit)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
pub(super) fn trust_file() -> PathBuf {
    store_path()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use sqlly_services::notebook::{Cell, CellKind, ConnectionRef, ExecutionPolicy};

    struct Root(#[allow(dead_code)] tempfile::TempDir);
    impl Root {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            super::super::autosave::set_test_root(Some(dir.path().to_path_buf()));
            Self(dir)
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            super::super::autosave::set_test_root(None);
        }
    }

    fn slot(id: &str, profile: &str) -> ConnectionRef {
        ConnectionRef {
            id: id.into(),
            profile_id: Some(profile.into()),
            label: id.to_uppercase(),
            database: None,
            extra: Default::default(),
        }
    }

    fn doc() -> NotebookDoc {
        let mut nb = NotebookDoc::empty("nbtrust", "Trust");
        nb.connection_refs = vec![slot("wh", "p-1"), slot("crm", "p-2")];
        for (n, slot) in ["wh", "crm"].iter().enumerate() {
            let mut cell = Cell::new(format!("c{n}"), CellKind::Sql, ExecutionPolicy::default());
            cell.source = format!("SELECT {n}");
            cell.connection_ref = Some(slot.to_string());
            nb.cells.push(cell);
        }
        nb
    }

    /// The docs-shots harness seeds `trust.json` from
    /// `scripts/docs-shots/fixtures/notebooks/trust-seeds.json` so its shots
    /// open their notebooks trusted; each seed must stay what the app records
    /// for that fixture created here.
    #[test]
    fn docs_shot_trust_seeds_match_their_fixtures() {
        let shots = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("scripts")
            .join("docs-shots");
        let fixtures = shots.join("fixtures").join("notebooks");
        let harness = std::fs::read_to_string(shots.join("shots.py")).unwrap();
        let ids: Vec<(String, String)> = harness
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(" = ")?;
                let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
                (name.starts_with("P_") && uuid::Uuid::parse_str(value).is_ok())
                    .then(|| (name.to_string(), value.to_string()))
            })
            .collect();
        assert!(ids.len() >= 3, "profile ids from shots.py: {ids:?}");
        let seeds: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            &std::fs::read_to_string(fixtures.join("trust-seeds.json")).unwrap(),
        )
        .unwrap();
        assert!(!seeds.is_empty());
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-18T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for (name, seed) in seeds {
            let mut text = std::fs::read_to_string(fixtures.join(&name)).unwrap();
            for (key, id) in &ids {
                text = text.replace(&format!("{{{key}}}"), id);
            }
            let load = |db| {
                sqlly_services::notebook::load(
                    Some(&name),
                    &text,
                    &mut sqlly_services::notebook::RandomIds,
                    db,
                )
                .unwrap()
                .doc
            };
            let doc = load(None);
            assert_eq!(
                trust::executable_hash(&doc),
                trust::executable_hash(&load(Some(sqlly_core::DatabaseType::SqlServer))),
                "{name}: the hash must not depend on the active connection"
            );
            let mut store = TrustStore::default();
            store.record_created(&doc, None, at);
            let mut record = serde_json::to_string(&store.notebooks[&doc.notebook_id][0]).unwrap();
            for (key, id) in &ids {
                record = record.replace(id.as_str(), &format!("{{{key}}}"));
            }
            let expected: serde_json::Value = serde_json::from_str(&record).unwrap();
            assert_eq!(
                seed,
                expected,
                "update the {name} entry of trust-seeds.json to:\n{}",
                serde_json::to_string_pretty(&expected).unwrap()
            );
        }
    }

    #[test]
    fn stores_sit_where_the_services_put_them() {
        assert_eq!(store_path(), TrustStore::default_path());
        assert_eq!(
            audit_dir(),
            sqlly_services::notebook_security::audit::AuditLog::default_dir()
        );
    }

    #[test]
    fn a_copied_origin_marker_grants_nothing() {
        let _root = Root::new();
        let mine = doc();
        record_created(&mine, None);
        let mut hostile = doc();
        hostile.notebook_id = "hostile".into();
        hostile.cells[0].source = "DROP TABLE dbo.Orders".into();
        hostile.extra.insert(
            "sqlly_origin".into(),
            serde_json::Value::String("0123456789abcdef".into()),
        );
        let opened = assess(&hostile, None).decision;
        assert_eq!(opened.level, TrustLevel::Untrusted);
        assert!(!opened.authorizes("wh", Some("p-1")));
    }

    #[test]
    fn saves_follow_in_app_edits_but_remaps_need_a_new_grant() {
        let _root = Root::new();
        let mut nb = doc();
        let base = record_created(&nb, None).decision;
        nb.cells[0].source = "SELECT 42".into();
        nb.connection_refs[1].profile_id = Some("p-9".into());
        nb.connection_refs.push(slot("new", "p-3"));
        let mut cell = Cell::new("c9", CellKind::Sql, ExecutionPolicy::default());
        cell.connection_ref = Some("new".into());
        nb.cells.push(cell);

        let in_session = live(&base, &nb);
        assert!(in_session.authorizes("wh", Some("p-1")));
        assert!(
            in_session.authorizes("new", Some("p-3")),
            "a slot added here"
        );
        assert!(
            !in_session.authorizes("crm", Some("p-9")),
            "a remapped slot"
        );

        let saved = record_save(&base, &nb, None);
        assert!(saved.problem.is_none());
        assert_eq!(saved.decision.connections, in_session.connections);
        assert_eq!(saved.decision.pending_connections, vec!["crm".to_string()]);
        let reopened = assess(&nb, None).decision;
        assert_eq!(reopened.level, TrustLevel::Trusted);
        assert!(!reopened.authorizes("crm", Some("p-9")));

        let granted = grant(
            &nb,
            &Grant {
                level: TrustLevel::Trusted,
                connections: ["wh", "crm", "new"].map(String::from).into(),
                allow_writes: false,
            },
            None,
        )
        .unwrap()
        .decision;
        assert!(granted.authorizes("crm", Some("p-9")));
        assert!(!granted.needs_banner());
    }

    #[test]
    fn a_second_tab_saving_the_same_notebook_keeps_its_trust() {
        let _root = Root::new();
        let mut nb = doc();
        let base = record_created(&nb, None).decision;
        let mut other = nb.clone();
        other.cells[0].source = "SELECT 'other tab'".into();
        record_save(&base, &other, None);
        nb.cells[0].source = "SELECT 'this tab'".into();
        let saved = record_save(&base, &nb, None).decision;
        assert_eq!(saved.level, TrustLevel::Trusted);
        assert_eq!(assess(&nb, None).decision.level, TrustLevel::Trusted);
    }

    #[test]
    fn browser_trust_is_kept_in_storage_and_a_damaged_entry_is_left_alone() {
        use sqlly_services::notebook::{MemoryTextStore, TextStore};
        use sqlly_services::notebook_security::trust::STORE_FILE;
        let storage = MemoryTextStore::default();
        let nb = doc();
        let (created, problem) = with_text_store(&storage, true, |store| {
            store.record_created(&nb, None, Utc::now())
        });
        assert!(problem.is_none());
        assert_eq!(created.level, TrustLevel::Trusted);
        let (reloaded, _) =
            with_text_store(&storage, false, |store| trust::assess(store, &nb, None));
        assert_eq!(reloaded.level, TrustLevel::Trusted);
        storage.write(STORE_FILE, "{ broken").unwrap();
        let (damaged, problem) =
            with_text_store(&storage, true, |store| trust::assess(store, &nb, None));
        assert!(problem.is_some());
        assert_eq!(damaged.level, TrustLevel::Untrusted);
        assert_eq!(
            storage.read(STORE_FILE).unwrap().as_deref(),
            Some("{ broken")
        );
    }

    #[test]
    fn a_new_notebook_drops_old_never_saved_ones_without_a_recovery_copy() {
        let _root = Root::new();
        let old = Utc::now() - chrono::Duration::days(trust::ABANDONED_AFTER_DAYS + 5);
        let mut ids = sqlly_services::notebook::SeededIds(41);
        let abandoned = NotebookDoc::new("Scratch", &mut ids);
        let recovered = NotebookDoc::new("Recovered", &mut ids);
        with_store(true, |store| {
            store.record_created(&abandoned, None, old);
            store.record_created(&recovered, None, old);
        });
        super::super::autosave::write_recovery(&recovered).unwrap();

        record_created(&doc(), None);

        let (ids, _) = with_store(false, |store| {
            store.notebooks.keys().cloned().collect::<Vec<_>>()
        });
        assert!(!ids.contains(&abandoned.notebook_id));
        assert!(ids.contains(&recovered.notebook_id));
        assert_eq!(assess(&recovered, None).decision.level, TrustLevel::Trusted);
    }

    #[test]
    fn a_damaged_trust_file_is_never_overwritten() {
        let _root = Root::new();
        let path = store_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let nb = doc();
        let opened = assess(&nb, None);
        assert_eq!(opened.decision.level, TrustLevel::Untrusted);
        assert!(opened.problem.is_some());
        let granted = grant(
            &nb,
            &Grant {
                level: TrustLevel::Restricted,
                connections: BTreeSet::from(["wh".to_string()]),
                allow_writes: true,
            },
            None,
        )
        .unwrap();
        assert!(granted.problem.is_some());
        assert_eq!(granted.decision.level, TrustLevel::Restricted);
        record_created(&doc(), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
