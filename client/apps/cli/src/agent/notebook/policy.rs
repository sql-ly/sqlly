//! What an unattended run may do, decided before anything executes.
//!
//! Read-only cells run on a copy of the profile with the SELECT-only gate
//! and the database's own read-only mode on (as the app's agent tools do).
//! Cells that change data are refused unless the run passes
//! `--allow-writes`, and never run on a production connection. Any cell on
//! a production connection needs `--allow-production`.

use sqlly_core::ConnectionProfile;
use sqlly_services::notebook::plan::StepAction;
use sqlly_services::notebook::{ExecClass, NotebookDoc, RunPlan};

use super::remap::{SlotTarget, Slots, Unmapped};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PolicyOptions {
    pub allow_writes: bool,
    pub allow_production: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    Unmapped {
        cell: String,
        unmapped: Unmapped,
    },
    /// A data-changing cell without `--allow-writes`.
    WriteCell {
        cell: String,
    },
    /// A data-changing cell on a production connection: always refused.
    WriteOnProduction {
        cell: String,
        connection: String,
    },
    /// A data-changing cell on the local workspace, which is read-only.
    LocalWrite {
        cell: String,
    },
    /// Any cell on a production connection without `--allow-production`.
    Production {
        cell: String,
        connection: String,
    },
}

impl Problem {
    pub fn message(&self) -> String {
        match self {
            Problem::Unmapped { cell, unmapped } => format!("cell {cell}: {}", unmapped.message()),
            Problem::WriteCell { cell } => {
                format!("cell {cell} changes data and the run does not allow writes")
            }
            Problem::WriteOnProduction { cell, connection } => format!(
                "cell {cell} changes data on the production connection '{connection}'; unattended runs never write to production"
            ),
            Problem::LocalWrite { cell } => {
                format!("cell {cell} changes data, but the notebook's local data is read-only")
            }
            Problem::Production { cell, connection } => {
                format!("cell {cell} runs on the production connection '{connection}'")
            }
        }
    }

    pub fn hint(&self) -> Option<String> {
        match self {
            Problem::Unmapped { unmapped, .. } => Some(unmapped.hint()),
            Problem::WriteCell { .. } => {
                Some("re-run with --allow-writes, or mark the cell read-only".into())
            }
            Problem::Production { .. } => Some("re-run with --allow-production".into()),
            Problem::WriteOnProduction { .. } | Problem::LocalWrite { .. } => None,
        }
    }
}

/// Every reason the plan's executing steps may not run.
pub fn preflight(
    doc: &NotebookDoc,
    plan: &RunPlan,
    slots: &Slots,
    options: PolicyOptions,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    for step in plan
        .steps
        .iter()
        .filter(|s| s.action == StepAction::Execute)
    {
        let Some(cell) = doc.cell(&step.cell_id) else {
            continue;
        };
        if cell.kind.exec_class() != ExecClass::Remote {
            continue;
        }
        let key = Slots::key_for(doc, cell);
        let label = cell.relation_name().to_string();
        let write = !cell.execution_policy.read_only;
        match slots.get(&key) {
            None => {
                if let Some(unmapped) = slots.unmapped.get(&key) {
                    problems.push(Problem::Unmapped {
                        cell: label,
                        unmapped: unmapped.clone(),
                    });
                }
            }
            Some(SlotTarget::Local) => {
                if write {
                    problems.push(Problem::LocalWrite { cell: label });
                }
            }
            Some(SlotTarget::Remote {
                profile,
                production,
            }) => {
                if write && *production {
                    problems.push(Problem::WriteOnProduction {
                        cell: label.clone(),
                        connection: profile.name.clone(),
                    });
                } else if write && !options.allow_writes {
                    problems.push(Problem::WriteCell {
                        cell: label.clone(),
                    });
                }
                if *production && !options.allow_production {
                    problems.push(Problem::Production {
                        cell: label,
                        connection: profile.name.clone(),
                    });
                }
            }
        }
    }
    problems
}

/// The profile a read-only cell runs under: the SELECT-only parser gate
/// plus the read-only mode the database enforces. The saved profile is
/// left untouched.
pub fn read_only_profile(profile: &ConnectionProfile) -> ConnectionProfile {
    let mut profile = profile.clone();
    profile.block_all_but_select = true;
    profile.enforce_read_only = true;
    profile
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::connections::StoreFile;
    use crate::agent::notebook::remap;
    use sqlly_core::{ConnectionEnvironment, SecretProvider, SecretRef};
    use sqlly_services::notebook::params::ResolvedParams;
    use sqlly_services::notebook::{
        CellKind, FingerprintInputs, Graph, NotebookState, PlanContext, RunRequest, SeededIds,
        fingerprint_all, plan,
    };
    use std::collections::BTreeMap;

    fn profile(name: &str, env: ConnectionEnvironment) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            name,
            "localhost",
            "u",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.name = name.into();
        p.environment = env;
        p
    }

    /// Plan "run all" for cells `(read_only, slot)` and return the problems.
    fn problems(
        cells: &[(bool, Option<&str>)],
        env: ConnectionEnvironment,
        options: PolicyOptions,
    ) -> Vec<Problem> {
        let saved = profile("Sales", env);
        let mut doc = NotebookDoc::empty("nb", "T");
        doc.connection_refs = vec![
            serde_json::from_value(serde_json::json!({
                "id": "primary", "profile_id": saved.id.0.to_string(), "label": "Sales"
            }))
            .unwrap(),
        ];
        let mut ids = SeededIds(9);
        for (i, (read_only, slot)) in cells.iter().enumerate() {
            let at = doc.add_cell(CellKind::Sql, None, &mut ids);
            let cell = &mut doc.cells[at];
            cell.name = Some(format!("cell{i}"));
            cell.source = if *read_only {
                "SELECT 1"
            } else {
                "DELETE FROM t"
            }
            .into();
            cell.execution_policy.read_only = *read_only;
            cell.connection_ref = slot.map(str::to_string);
        }
        let store = StoreFile {
            version: 1,
            profiles: vec![saved],
            environments: vec![],
        };
        let slots = remap::resolve(&doc, &[], &store).unwrap();
        let dialects = slots.dialects();
        let graph = Graph::build(&doc, &dialects);
        let params = ResolvedParams::default();
        let identities = slots.identities();
        let overrides = BTreeMap::new();
        let fps = fingerprint_all(
            &doc,
            &graph,
            &FingerprintInputs {
                dialects: &dialects,
                connections: &identities,
                params: &params,
                upstream_override: &overrides,
            },
        );
        let state = NotebookState::default();
        let p = plan(
            &RunRequest::All,
            &PlanContext {
                doc: &doc,
                graph: &graph,
                fingerprints: &fps,
                state: &state,
                params: &params,
                now_ms: 0,
            },
        );
        preflight(&doc, &p, &slots, options)
    }

    fn code(problem: &Problem) -> &'static str {
        match problem {
            Problem::Unmapped { .. } => "unmapped_connection",
            Problem::WriteCell { .. } => "write_cell",
            Problem::WriteOnProduction { .. } => "write_on_production",
            Problem::LocalWrite { .. } => "local_write",
            Problem::Production { .. } => "production_connection",
        }
    }

    fn codes(problems: &[Problem]) -> Vec<&'static str> {
        problems.iter().map(code).collect()
    }

    #[test]
    fn read_only_cells_on_dev_pass() {
        assert!(
            problems(
                &[(true, None)],
                ConnectionEnvironment::Dev,
                PolicyOptions::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn write_cells_need_allow_writes() {
        let refused = problems(
            &[(false, None)],
            ConnectionEnvironment::Dev,
            PolicyOptions::default(),
        );
        assert_eq!(codes(&refused), ["write_cell"]);
        assert!(refused[0].hint().unwrap().contains("--allow-writes"));
        let allowed = PolicyOptions {
            allow_writes: true,
            ..PolicyOptions::default()
        };
        assert!(problems(&[(false, None)], ConnectionEnvironment::Dev, allowed).is_empty());
    }

    #[test]
    fn production_needs_its_flag_and_never_takes_writes() {
        let read = problems(
            &[(true, None)],
            ConnectionEnvironment::Production,
            PolicyOptions::default(),
        );
        assert_eq!(codes(&read), ["production_connection"]);
        let everything = PolicyOptions {
            allow_writes: true,
            allow_production: true,
        };
        let write = problems(
            &[(false, None)],
            ConnectionEnvironment::Production,
            everything,
        );
        assert_eq!(codes(&write), ["write_on_production"]);
        assert!(write[0].hint().is_none());
    }

    #[test]
    fn unmapped_and_local_writes_are_reported() {
        let found = problems(
            &[(true, Some("ghost")), (false, Some(remap::LOCAL_SLOT))],
            ConnectionEnvironment::Dev,
            PolicyOptions {
                allow_writes: true,
                ..PolicyOptions::default()
            },
        );
        assert_eq!(codes(&found), ["unmapped_connection", "local_write"]);
        assert!(found[0].message().starts_with("cell cell0:"));
    }

    #[test]
    fn read_only_profile_sets_both_layers() {
        let p = read_only_profile(&profile("x", ConnectionEnvironment::Dev));
        assert!(p.block_all_but_select && p.enforce_read_only);
    }
}
