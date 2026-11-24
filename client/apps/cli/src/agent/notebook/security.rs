//! The runner's side of lane 9's notebook security: trust, per-cell
//! authorization and audit events.
//!
//! Trust is assessed against the notebook as it will run: the file with
//! each connection slot pointing at the profile this run maps it to. So a
//! notebook trusted in the app runs as trusted when nothing is remapped,
//! and a remap (or an edit outside the app) needs a fresh grant. `--trust`
//! grants it for one run without saving; `schedule add --trust` saves the
//! grant, which scheduled runs then need to find unchanged.
//!
//! SQL cells on the `local` slot read only the notebook's own read-only
//! workspace, so they are not a connection to authorize; they are still
//! audited.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::Utc;
use sqlly_core::ConnectionEnvironment;
use sqlly_services::notebook::plan::StepAction;
use sqlly_services::notebook::{CellKind, NotebookDoc, RunPlan};
use sqlly_services::notebook_security::audit::{
    AuditAction, AuditEvent, AuditLog, AuditOutcome, AuditPrivacy, ExportAudit,
};
use sqlly_services::notebook_security::authorize::{
    AuthContext, ConnectionFacts, Consents, DataPolicy, Decision, NotebookAction, authorize,
};
use sqlly_services::notebook_security::trust::{
    self, Grant, TrustDecision, TrustLevel, TrustStore,
};

use super::remap::{LOCAL_SLOT, SlotTarget, Slots};
use crate::agent::CliError;

/// `doc` with each mapped slot's profile id set to the profile this run
/// uses.
pub fn effective_doc(doc: &NotebookDoc, slots: &Slots) -> NotebookDoc {
    let mut out = doc.clone();
    for slot in &mut out.connection_refs {
        if let Some(SlotTarget::Remote { profile, .. }) = slots.get(&slot.id) {
            slot.profile_id = Some(profile.id.0.to_string());
        }
    }
    out
}

pub fn facts(slots: &Slots) -> BTreeMap<String, ConnectionFacts> {
    slots
        .targets
        .iter()
        .filter_map(|(key, target)| match target {
            SlotTarget::Remote { profile, .. } => Some((
                key.clone(),
                ConnectionFacts {
                    profile_id: Some(profile.id.0.to_string()),
                    engine: Some(profile.database_type),
                    environment: profile.environment,
                    allow_local_materialize: true,
                    writes_unlocked: false,
                },
            )),
            SlotTarget::Local => None,
        })
        .collect()
}

fn load_store(path: &Path) -> TrustStore {
    TrustStore::load(path).unwrap_or_else(|e| {
        eprintln!("warning: {}", e.message().fallback());
        TrustStore::default()
    })
}

/// Every slot the notebook can run on, for a grant.
fn grant_for(doc: &NotebookDoc, allow_writes: bool) -> Grant {
    Grant {
        level: TrustLevel::Trusted,
        connections: trust::declared_slots(doc),
        allow_writes,
    }
}

/// The trust this run has: from the store, or granted for this run only.
pub fn trust_for_run(
    doc: &NotebookDoc,
    file: &Path,
    store_path: &Path,
    trust_once: bool,
    allow_writes: bool,
) -> Result<TrustDecision, CliError> {
    let mut store = load_store(store_path);
    if trust_once {
        return trust::grant(
            &mut store,
            doc,
            &grant_for(doc, allow_writes),
            Some(file),
            Utc::now(),
        )
        .map_err(|e| CliError::usage(e.message().fallback().to_string()));
    }
    Ok(trust::assess(&store, doc, Some(file)))
}

/// Grant and save trust (for `schedule add --trust`).
pub fn remember_trust(
    doc: &NotebookDoc,
    file: &Path,
    store_path: &Path,
) -> Result<TrustDecision, CliError> {
    let mut store = load_store(store_path);
    let decision = trust::grant(
        &mut store,
        doc,
        &grant_for(doc, false),
        Some(file),
        Utc::now(),
    )
    .map_err(|e| CliError::usage(e.message().fallback().to_string()))?;
    store
        .save(store_path)
        .map_err(|e| CliError::io(e.message().fallback().to_string()))?;
    Ok(decision)
}

pub fn assess(doc: &NotebookDoc, file: &Path, store_path: &Path) -> TrustDecision {
    trust::assess(&load_store(store_path), doc, Some(file))
}

/// Where `doc`'s runs and local data live: its trust record's scope, else
/// the one a grant for `file` gets. Never just the id the file declares.
pub fn data_scope(doc: &NotebookDoc, file: &Path, store_path: &Path) -> String {
    let store = load_store(store_path);
    trust::assess(&store, doc, Some(file))
        .scope
        .unwrap_or_else(|| store.scope_for_grant(&doc.notebook_id, Some(file)))
}

/// One cell's authorization, or why it was refused.
pub struct CellCheck {
    pub cell_id: String,
    pub label: String,
    pub decision: Decision,
}

/// `authorize(RunCell)` for every step the plan executes. Unattended runs
/// pass no consents beyond `--allow-writes`, so "needs approval" refuses.
pub fn check_cells(
    doc: &NotebookDoc,
    plan: &RunPlan,
    slots: &Slots,
    decision: &TrustDecision,
    elevate_writes: bool,
) -> Vec<CellCheck> {
    let connections = facts(slots);
    let policy = DataPolicy::default();
    let consents = Consents {
        elevated_cells: if elevate_writes {
            doc.cells
                .iter()
                .filter(|c| !c.execution_policy.read_only)
                .map(|c| c.id.clone())
                .collect()
        } else {
            BTreeSet::new()
        },
        ..Consents::default()
    };
    let ctx = AuthContext {
        doc,
        trust: decision,
        connections: &connections,
        policy: &policy,
        consents: &consents,
    };
    plan.steps
        .iter()
        .filter(|s| s.action == StepAction::Execute)
        .filter_map(|step| {
            let cell = doc.cell(&step.cell_id)?;
            let decision = if cell.kind == CellKind::Sql
                && Slots::key_for(doc, cell) == LOCAL_SLOT
                && decision.level != TrustLevel::Untrusted
            {
                Decision::Allow {
                    notices: Vec::new(),
                }
            } else {
                authorize(&NotebookAction::RunCell { cell_id: &cell.id }, &ctx)
            };
            Some(CellCheck {
                cell_id: cell.id.clone(),
                label: cell.relation_name().to_string(),
                decision,
            })
        })
        .collect()
}

pub fn schedule_decision(
    doc: &NotebookDoc,
    slots: &Slots,
    trust: &TrustDecision,
    allow_production: bool,
) -> Decision {
    let connections = facts(slots);
    let policy = DataPolicy::default();
    let consents = Consents::default();
    authorize(
        &NotebookAction::Schedule { allow_production },
        &AuthContext {
            doc,
            trust,
            connections: &connections,
            policy: &policy,
            consents: &consents,
        },
    )
}

/// Audit events for one run. Failures to write are warnings: the run's
/// own record is the source of truth for its outcome.
pub struct Auditor {
    log: AuditLog,
    actor: String,
    notebook_id: String,
    version: String,
}

pub struct CellAudit<'a> {
    pub cell_id: &'a str,
    pub decision: &'a Decision,
    pub profile_id: Option<&'a str>,
    pub environment: ConnectionEnvironment,
    pub sql: Option<&'a str>,
    pub rows: Option<u64>,
    pub duration_ms: Option<u64>,
    pub outcome: Option<AuditOutcome>,
    pub error: Option<&'a str>,
}

impl Auditor {
    pub fn new(dir: &Path, actor: &str, notebook_id: &str, version: &str) -> Self {
        Self {
            log: AuditLog::new(dir),
            actor: actor.to_string(),
            notebook_id: notebook_id.to_string(),
            version: version.to_string(),
        }
    }

    fn append(&self, event: AuditEvent) {
        if let Err(e) = self.log.append(&event) {
            eprintln!("warning: could not write the audit log: {e}");
        }
    }

    pub fn cell(&self, entry: CellAudit<'_>) {
        let mut event = AuditEvent::new(
            Utc::now(),
            self.actor.clone(),
            self.notebook_id.clone(),
            self.version.clone(),
            AuditAction::RunCell,
            entry.decision,
        )
        .with_cell(entry.cell_id)
        .with_connection(entry.profile_id, entry.environment)
        .with_counts(entry.rows, None, entry.duration_ms);
        if let Some(sql) = entry.sql {
            event = event.with_sql(sql, &AuditPrivacy::default());
        }
        if let Some(outcome) = entry.outcome {
            event = event.with_outcome(outcome, entry.error);
        }
        self.append(event);
    }

    pub fn export(&self, decision: &Decision, details: ExportAudit) {
        self.append(
            AuditEvent::new(
                Utc::now(),
                self.actor.clone(),
                self.notebook_id.clone(),
                self.version.clone(),
                AuditAction::Export,
                decision,
            )
            .with_export(details),
        );
    }

    pub fn schedule(&self, decision: &Decision) {
        self.append(AuditEvent::new(
            Utc::now(),
            self.actor.clone(),
            self.notebook_id.clone(),
            self.version.clone(),
            AuditAction::Schedule,
            decision,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::connections::StoreFile;
    use crate::agent::notebook::remap;
    use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};

    fn setup() -> (NotebookDoc, Slots) {
        let mut profile = ConnectionProfile::sql_login(
            "Sales",
            "localhost",
            "u",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.name = "Sales".into();
        let doc: NotebookDoc = serde_json::from_value(serde_json::json!({
            "format": "sqlly.notebook", "format_version": 2, "notebook_id": "nb_sec", "title": "T",
            "connection_refs": [{"id": "primary", "label": "Sales"}],
            "cells": [{"id": "c_1", "kind": "sql", "source": "SELECT 1"}]
        }))
        .unwrap();
        let store = StoreFile {
            version: 1,
            profiles: vec![profile],
            environments: vec![],
        };
        let mappings = remap::parse_mappings(&doc, &["primary=Sales".into()]).unwrap();
        let slots = remap::resolve(&doc, &mappings, &store).unwrap();
        (doc, slots)
    }

    #[test]
    fn untrusted_notebooks_need_a_grant_and_remaps_change_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("trust.json");
        let file = dir.path().join("nb.sqllynb");
        let (doc, slots) = setup();
        let effective = effective_doc(&doc, &slots);
        assert_ne!(
            trust::executable_hash(&doc),
            trust::executable_hash(&effective)
        );
        let decision = trust_for_run(&effective, &file, &store_path, false, false).unwrap();
        assert_eq!(decision.level, TrustLevel::Untrusted);
        let plan = sqlly_services::notebook::RunPlan {
            steps: vec![sqlly_services::notebook::plan::PlanStep {
                cell_id: "c_1".into(),
                action: StepAction::Execute,
                lane: sqlly_services::notebook::plan::Lane::Connection {
                    slot: Some("primary".into()),
                },
                depends_on: vec![],
                fingerprint: None,
                requires_approval: false,
                explicit: true,
            }],
            ..Default::default()
        };
        let checks = check_cells(&effective, &plan, &slots, &decision, false);
        assert!(!checks[0].decision.is_allowed());
        // Granted for one run: allowed, and nothing saved.
        let once = trust_for_run(&effective, &file, &store_path, true, false).unwrap();
        assert!(
            check_cells(&effective, &plan, &slots, &once, false)[0]
                .decision
                .is_allowed()
        );
        assert!(!store_path.exists());
        // Remembered: a later run finds it without --trust.
        remember_trust(&effective, &file, &store_path).unwrap();
        let later = assess(&effective, &file, &store_path);
        assert!(
            check_cells(&effective, &plan, &slots, &later, false)[0]
                .decision
                .is_allowed()
        );
        assert!(schedule_decision(&effective, &slots, &later, false).is_allowed());
        // The SQL changed outside the app: trust is gone.
        let mut edited = effective.clone();
        edited.cells[0].source = "SELECT 2".into();
        assert_eq!(
            assess(&edited, &file, &store_path).level,
            TrustLevel::Untrusted
        );
    }

    #[test]
    fn untrusted_notebooks_run_no_local_cells_either() {
        let dir = tempfile::tempdir().unwrap();
        let (mut doc, slots) = setup();
        let mut local = doc.cells[0].clone();
        local.id = "c_local".into();
        local.source = "SELECT * FROM \"orders\"".into();
        local.connection_ref = Some(LOCAL_SLOT.into());
        doc.cells.push(local);
        let effective = effective_doc(&doc, &slots);
        let file = dir.path().join("nb.sqllynb");
        let store_path = dir.path().join("trust.json");
        let step = sqlly_services::notebook::plan::PlanStep {
            cell_id: "c_local".into(),
            action: StepAction::Execute,
            lane: sqlly_services::notebook::plan::Lane::Local,
            depends_on: vec![],
            fingerprint: None,
            requires_approval: false,
            explicit: true,
        };
        let plan = sqlly_services::notebook::RunPlan {
            steps: vec![step],
            ..Default::default()
        };
        let untrusted = trust_for_run(&effective, &file, &store_path, false, false).unwrap();
        assert_eq!(untrusted.level, TrustLevel::Untrusted);
        let checks = check_cells(&effective, &plan, &slots, &untrusted, false);
        assert!(
            !checks[0].decision.is_allowed(),
            "a hostile file must not read the workspace of the notebook it names"
        );
        let once = trust_for_run(&effective, &file, &store_path, true, false).unwrap();
        assert!(
            check_cells(&effective, &plan, &slots, &once, false)[0]
                .decision
                .is_allowed()
        );
    }

    #[test]
    fn audit_lines_carry_hashes_not_sql() {
        let dir = tempfile::tempdir().unwrap();
        let auditor = Auditor::new(dir.path(), "me (sqlly CLI)", "nb", "h");
        let allow = Decision::Allow { notices: vec![] };
        auditor.cell(CellAudit {
            cell_id: "c_1",
            decision: &allow,
            profile_id: Some("p"),
            environment: ConnectionEnvironment::Dev,
            sql: Some("SELECT secret_column FROM t"),
            rows: Some(3),
            duration_ms: Some(5),
            outcome: Some(AuditOutcome::Completed),
            error: None,
        });
        let text = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
        assert!(text.contains("\"query_hash\""), "{text}");
        assert!(!text.contains("secret_column"), "{text}");
    }
}
