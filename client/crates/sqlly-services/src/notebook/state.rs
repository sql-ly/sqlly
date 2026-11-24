//! Per-cell output state: a pure reducer over [`CellEvent`]s plus the
//! notebook-wide rules (a failed or canceled upstream blocks waiting
//! dependents; an invalidated cell makes its dependents stale; outputs whose
//! fingerprint no longer matches go stale). Stale content is never
//! presented as current: check [`CellState::is_presentable_as_current`].
//!
//! Lifecycle: `idle → validating → waiting → awaiting_approval? → queued →
//! running → streaming → fresh | cached | partial | failed | canceled`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlly_lang::DisplayMessage;

use super::document::{CachePolicy, NotebookDoc};
use super::fingerprint::{CellFingerprint, ChangeReason};
use super::graph::{Graph, Node};
use super::output::{Completeness, Freshness};

/// The output states of contract #4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellStatus {
    #[default]
    Idle,
    Validating,
    Waiting,
    AwaitingApproval,
    Queued,
    Running,
    Streaming,
    Fresh,
    Cached,
    Stale,
    Canceled,
    Blocked,
    Failed,
    Partial,
    Unavailable,
}

impl CellStatus {
    /// Work is in flight for the cell.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            CellStatus::Validating
                | CellStatus::Waiting
                | CellStatus::AwaitingApproval
                | CellStatus::Queued
                | CellStatus::Running
                | CellStatus::Streaming
        )
    }

    /// The status follows from the output alone (no failure, cancel,
    /// block or unavailability to keep showing).
    fn is_resting(self) -> bool {
        matches!(
            self,
            CellStatus::Idle
                | CellStatus::Fresh
                | CellStatus::Cached
                | CellStatus::Stale
                | CellStatus::Partial
        )
    }

    fn is_executing(self) -> bool {
        matches!(self, CellStatus::Running | CellStatus::Streaming)
    }
}

/// What is known about the cell's last output (the rows live in
/// [`super::output::CellOutput`], held by the host).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputMeta {
    pub run_id: String,
    /// The fingerprint the output was produced under.
    pub fingerprint: CellFingerprint,
    pub completeness: Completeness,
    pub row_count: u64,
    /// Unix milliseconds.
    pub finished_at_ms: i64,
    /// Restored from a cache rather than produced by this run.
    pub from_cache: bool,
    pub freshness: Freshness,
}

/// Why an output stopped being current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    /// Its fingerprint changed (source, parameters, connection, policy or
    /// upstream). Reverting the change makes it current again.
    Changed(Vec<ChangeReason>),
    /// An upstream cell was invalidated.
    UpstreamInvalidated { cell: String },
    /// The TTL elapsed.
    Expired,
    /// The cache was cleared or the user marked it stale.
    Cleared,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellState {
    pub status: CellStatus,
    /// The last output, which may be stale or partial.
    pub output: Option<OutputMeta>,
    /// The run in flight.
    pub run_id: Option<String>,
    pub rows_streamed: u64,
    pub error: Option<DisplayMessage>,
    pub blocked_by: Option<String>,
    pub stale: Option<StaleReason>,
    /// Set on cancel: whether the server confirmed it.
    pub cancel_confirmed: Option<bool>,
    /// An invalidation arrived while the current run was executing, so its
    /// result is stale on arrival.
    pub invalidated_mid_run: bool,
}

/// Events that drive the reducer.
#[derive(Debug, Clone, PartialEq)]
pub enum CellEvent {
    ValidationStarted,
    /// Validation finished; `error` is the first problem, if any.
    Validated {
        error: Option<DisplayMessage>,
    },
    Waiting,
    AwaitingApproval,
    Queued,
    Started {
        run_id: String,
    },
    Streaming {
        rows: u64,
    },
    Completed {
        output: OutputMeta,
    },
    Failed {
        error: DisplayMessage,
    },
    Canceled {
        server_confirmed: bool,
    },
    Invalidated {
        reason: StaleReason,
    },
    Blocked {
        by: String,
    },
    Unavailable {
        reason: DisplayMessage,
    },
    Reset,
}

/// An event that does not apply in the cell's current status.
#[derive(Debug, Clone, PartialEq)]
pub struct InvalidTransition {
    pub from: CellStatus,
    pub event: Box<CellEvent>,
}

impl CellState {
    /// The status the cell rests in given its output.
    fn resting(&self) -> CellStatus {
        match (&self.output, &self.stale) {
            (None, _) => CellStatus::Idle,
            (Some(_), Some(_)) => CellStatus::Stale,
            (Some(o), None) if o.completeness == Completeness::Partial => CellStatus::Partial,
            (Some(o), None) if o.from_cache => CellStatus::Cached,
            (Some(_), None) => CellStatus::Fresh,
        }
    }

    /// The output may be shown without a stale/partial warning.
    pub fn is_presentable_as_current(&self) -> bool {
        matches!(self.status, CellStatus::Fresh | CellStatus::Cached)
            && self.stale.is_none()
            && self
                .output
                .as_ref()
                .is_some_and(|o| o.completeness != Completeness::Partial)
    }

    /// The output is current for `fingerprint`.
    pub fn is_current_for(&self, fingerprint: &CellFingerprint) -> bool {
        self.is_presentable_as_current()
            && self
                .output
                .as_ref()
                .is_some_and(|o| o.fingerprint.value == fingerprint.value)
    }

    /// Apply one event. The state is unchanged on error.
    pub fn reduce(&self, event: CellEvent) -> Result<CellState, InvalidTransition> {
        use CellStatus as S;
        let from = self.status;
        let reject = |event| {
            Err(InvalidTransition {
                from,
                event: Box::new(event),
            })
        };
        let mut next = self.clone();
        match &event {
            CellEvent::ValidationStarted if !from.is_executing() => {
                next.status = S::Validating;
            }
            CellEvent::Validated { error } if from == S::Validating => match error {
                Some(error) => {
                    next.status = S::Failed;
                    next.error = Some(error.clone());
                }
                None => {
                    next.error = None;
                    next.status = next.resting();
                }
            },
            CellEvent::Waiting if !from.is_executing() => {
                next.status = S::Waiting;
                next.blocked_by = None;
            }
            CellEvent::AwaitingApproval if !from.is_executing() => {
                next.status = S::AwaitingApproval;
            }
            CellEvent::Queued if !from.is_executing() => {
                next.status = S::Queued;
                next.blocked_by = None;
            }
            CellEvent::Started { run_id } if !from.is_executing() => {
                next.status = S::Running;
                next.run_id = Some(run_id.clone());
                next.rows_streamed = 0;
                next.error = None;
                next.blocked_by = None;
                next.cancel_confirmed = None;
                next.invalidated_mid_run = false;
            }
            CellEvent::Streaming { rows } if from.is_executing() => {
                next.status = S::Streaming;
                next.rows_streamed = *rows;
            }
            CellEvent::Completed { output }
                if (from.is_executing()
                    && next.run_id.as_deref() == Some(output.run_id.as_str()))
                    || (output.from_cache && !from.is_executing()) =>
            {
                // An edit that landed mid-run keeps its stale mark.
                let stale_mid_run = from.is_executing() && next.invalidated_mid_run;
                next.output = Some(output.clone());
                next.run_id = None;
                next.error = None;
                next.invalidated_mid_run = false;
                if !stale_mid_run {
                    next.stale = None;
                }
                next.status = next.resting();
            }
            CellEvent::Failed { error } if from.is_active() || from == S::Idle => {
                next.status = S::Failed;
                next.error = Some(error.clone());
                next.run_id = None;
            }
            CellEvent::Canceled { server_confirmed } if from.is_active() => {
                next.status = S::Canceled;
                next.cancel_confirmed = Some(*server_confirmed);
                next.run_id = None;
            }
            CellEvent::Invalidated { reason } => {
                if next.output.is_some() || from.is_executing() {
                    next.stale = Some(reason.clone());
                }
                next.invalidated_mid_run |= from.is_executing();
                if !from.is_active() && next.output.is_some() {
                    next.status = S::Stale;
                }
            }
            CellEvent::Blocked { by } if !from.is_executing() => {
                next.status = S::Blocked;
                next.blocked_by = Some(by.clone());
            }
            CellEvent::Unavailable { reason } if !from.is_executing() => {
                next.status = S::Unavailable;
                next.error = Some(reason.clone());
            }
            CellEvent::Reset => next = CellState::default(),
            _ => return reject(event),
        }
        Ok(next)
    }
}

/// State of every cell in a notebook.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NotebookState {
    pub cells: BTreeMap<String, CellState>,
}

impl NotebookState {
    pub fn get(&self, cell_id: &str) -> CellState {
        self.cells.get(cell_id).cloned().unwrap_or_default()
    }

    fn set(&mut self, cell_id: &str, state: CellState) -> bool {
        let changed = self.cells.get(cell_id) != Some(&state);
        self.cells.insert(cell_id.to_string(), state);
        changed
    }

    /// Apply `event` to `cell_id` and the rules that follow from it. Returns
    /// every cell whose state changed, the target first.
    pub fn apply(
        &mut self,
        graph: &Graph,
        cell_id: &str,
        event: CellEvent,
    ) -> Result<Vec<String>, InvalidTransition> {
        let next = self.get(cell_id).reduce(event.clone())?;
        let mut changed = Vec::new();
        if self.set(cell_id, next) {
            changed.push(cell_id.to_string());
        }
        let downstream: Vec<String> = graph
            .downstream_closure(&Node::cell(cell_id))
            .into_iter()
            .filter_map(|n| n.as_cell().map(str::to_string))
            .collect();
        for id in downstream {
            let state = self.get(&id);
            let follow = match &event {
                CellEvent::Failed { .. }
                | CellEvent::Canceled { .. }
                | CellEvent::Blocked { .. }
                    if matches!(
                        state.status,
                        CellStatus::Waiting | CellStatus::Queued | CellStatus::AwaitingApproval
                    ) =>
                {
                    Some(CellEvent::Blocked {
                        by: cell_id.to_string(),
                    })
                }
                CellEvent::Invalidated { .. }
                    if state.output.is_some() && state.stale.is_none() =>
                {
                    Some(CellEvent::Invalidated {
                        reason: StaleReason::UpstreamInvalidated {
                            cell: cell_id.to_string(),
                        },
                    })
                }
                _ => None,
            };
            if let Some(follow) = follow
                && let Ok(next) = state.reduce(follow)
                && self.set(&id, next)
            {
                changed.push(id);
            }
        }
        Ok(changed)
    }

    /// Compare every output with the cells' current fingerprints: mismatches
    /// go stale (with the reasons), and outputs whose fingerprint matches
    /// again after a reverted change become current again. Returns the
    /// cells whose state changed.
    pub fn refresh_staleness(
        &mut self,
        fingerprints: &BTreeMap<String, CellFingerprint>,
    ) -> Vec<String> {
        let mut changed = Vec::new();
        let ids: Vec<String> = self.cells.keys().cloned().collect();
        for id in ids {
            let state = self.get(&id);
            let (Some(output), Some(current)) = (&state.output, fingerprints.get(&id)) else {
                continue;
            };
            let mut next = state.clone();
            if output.fingerprint.value != current.value {
                let reasons = current.changes_from(&output.fingerprint);
                if !matches!(&state.stale, Some(StaleReason::Changed(r)) if *r == reasons)
                    && !matches!(
                        &state.stale,
                        Some(StaleReason::Expired | StaleReason::Cleared)
                    )
                {
                    next.stale = Some(StaleReason::Changed(reasons));
                }
            } else if matches!(state.stale, Some(StaleReason::Changed(_))) {
                next.stale = None;
            }
            if next.status.is_resting() {
                next.status = next.resting();
            }
            if self.set(&id, next) {
                changed.push(id);
            }
        }
        changed
    }

    /// Mark outputs older than their cell's TTL stale.
    pub fn expire(&mut self, doc: &NotebookDoc, now_ms: i64) -> Vec<String> {
        let mut changed = Vec::new();
        for cell in &doc.cells {
            let CachePolicy::Ttl { seconds } = cell.execution_policy.cache else {
                continue;
            };
            let state = self.get(&cell.id);
            let expired = state.output.as_ref().is_some_and(|o| {
                now_ms.saturating_sub(o.finished_at_ms) > (seconds as i64).saturating_mul(1000)
            });
            if expired
                && state.stale.is_none()
                && let Ok(next) = state.reduce(CellEvent::Invalidated {
                    reason: StaleReason::Expired,
                })
                && self.set(&cell.id, next)
            {
                changed.push(cell.id.clone());
            }
        }
        changed
    }

    /// After a crash or restart nothing is in flight: every active cell
    /// becomes canceled (unconfirmed).
    pub fn reconcile_interrupted(&mut self) -> Vec<String> {
        let mut changed = Vec::new();
        for (id, state) in self.cells.iter_mut() {
            if state.status.is_active() {
                state.status = CellStatus::Canceled;
                state.cancel_confirmed = Some(false);
                state.run_id = None;
                changed.push(id.clone());
            }
        }
        changed
    }

    /// Drop state for cells no longer in the document.
    pub fn retain(&mut self, doc: &NotebookDoc) {
        self.cells.retain(|id, _| doc.cell(id).is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{Cell, CellKind, SeededIds};
    use crate::notebook::fingerprint::{Fingerprint, FingerprintParts};
    use crate::notebook::references::Dialects;

    fn fp(tag: &str) -> CellFingerprint {
        let part = |p: &str| Fingerprint(format!("{tag}-{p}"));
        CellFingerprint {
            value: Fingerprint(tag.into()),
            parts: FingerprintParts {
                source: part("s"),
                kind_spec: part("k"),
                connection: Fingerprint("c".into()),
                policy: Fingerprint("p".into()),
                params: Fingerprint("q".into()),
                upstream: Fingerprint("u".into()),
            },
        }
    }

    fn meta(run: &str, tag: &str) -> OutputMeta {
        OutputMeta {
            run_id: run.into(),
            fingerprint: fp(tag),
            completeness: Completeness::Complete,
            row_count: 3,
            finished_at_ms: 1_000,
            from_cache: false,
            freshness: Freshness::Live,
        }
    }

    fn msg() -> DisplayMessage {
        DisplayMessage::raw("boom")
    }

    fn run(state: &CellState, events: Vec<CellEvent>) -> CellState {
        events
            .into_iter()
            .fold(state.clone(), |s, e| s.reduce(e).expect("valid transition"))
    }

    #[test]
    fn happy_path_reaches_fresh() {
        let s = run(
            &CellState::default(),
            vec![
                CellEvent::ValidationStarted,
                CellEvent::Validated { error: None },
                CellEvent::Waiting,
                CellEvent::AwaitingApproval,
                CellEvent::Queued,
                CellEvent::Started {
                    run_id: "r1".into(),
                },
                CellEvent::Streaming { rows: 10 },
                CellEvent::Completed {
                    output: meta("r1", "a"),
                },
            ],
        );
        assert_eq!(s.status, CellStatus::Fresh);
        assert!(s.is_presentable_as_current());
        assert!(s.is_current_for(&fp("a")));
        assert!(!s.is_current_for(&fp("b")));
    }

    #[test]
    fn late_completions_from_old_runs_are_rejected() {
        let s = run(
            &CellState::default(),
            vec![CellEvent::Started {
                run_id: "r2".into(),
            }],
        );
        let err = s
            .reduce(CellEvent::Completed {
                output: meta("r1", "a"),
            })
            .unwrap_err();
        assert_eq!(err.from, CellStatus::Running);
        assert!(
            CellState::default()
                .reduce(CellEvent::Streaming { rows: 1 })
                .is_err()
        );
        assert!(
            CellState::default()
                .reduce(CellEvent::Canceled {
                    server_confirmed: true
                })
                .is_err()
        );
    }

    #[test]
    fn cache_hits_partial_results_and_cancels() {
        let mut cached = meta("r0", "a");
        cached.from_cache = true;
        let s = run(
            &CellState::default(),
            vec![CellEvent::Queued, CellEvent::Completed { output: cached }],
        );
        assert_eq!(s.status, CellStatus::Cached);
        assert!(s.is_presentable_as_current());
        let mut partial = meta("r1", "a");
        partial.completeness = Completeness::Partial;
        let s = run(
            &CellState::default(),
            vec![
                CellEvent::Started {
                    run_id: "r1".into(),
                },
                CellEvent::Completed { output: partial },
            ],
        );
        assert_eq!(s.status, CellStatus::Partial);
        assert!(!s.is_presentable_as_current());
        let s = run(
            &CellState::default(),
            vec![
                CellEvent::Started {
                    run_id: "r1".into(),
                },
                CellEvent::Canceled {
                    server_confirmed: false,
                },
            ],
        );
        assert_eq!(s.status, CellStatus::Canceled);
        assert_eq!(s.cancel_confirmed, Some(false));
        assert!(!s.is_presentable_as_current());
    }

    #[test]
    fn failures_keep_the_old_output_but_never_present_it() {
        let s = run(
            &CellState::default(),
            vec![
                CellEvent::Started {
                    run_id: "r1".into(),
                },
                CellEvent::Completed {
                    output: meta("r1", "a"),
                },
                CellEvent::Started {
                    run_id: "r2".into(),
                },
                CellEvent::Failed { error: msg() },
            ],
        );
        assert_eq!(s.status, CellStatus::Failed);
        assert!(s.output.is_some());
        assert!(!s.is_presentable_as_current());

        // Re-checking staleness (every edit, every plan) keeps a failure,
        // cancel or block on show and never turns it back into Fresh.
        let (_, graph) = chain();
        for last in [
            CellEvent::Failed { error: msg() },
            CellEvent::Canceled {
                server_confirmed: true,
            },
            CellEvent::Blocked { by: "x".into() },
        ] {
            let mut nb = NotebookState::default();
            for event in [
                CellEvent::Started {
                    run_id: "r1".into(),
                },
                CellEvent::Completed {
                    output: meta("r1", "a"),
                },
                CellEvent::Queued,
                last.clone(),
            ] {
                nb.apply(&graph, "a", event).unwrap();
            }
            let before = nb.get("a").status;
            for tag in ["a", "a2"] {
                nb.refresh_staleness(&BTreeMap::from([("a".to_string(), fp(tag))]));
                assert_eq!(nb.get("a").status, before, "{last:?} under {tag}");
                assert!(!nb.get("a").is_presentable_as_current());
            }
        }
    }

    #[test]
    fn edits_during_a_run_leave_the_result_stale() {
        let s = run(
            &CellState::default(),
            vec![
                CellEvent::Started {
                    run_id: "r1".into(),
                },
                CellEvent::Invalidated {
                    reason: StaleReason::Changed(vec![ChangeReason::Source]),
                },
                CellEvent::Completed {
                    output: meta("r1", "a"),
                },
            ],
        );
        assert_eq!(s.status, CellStatus::Stale);
        assert!(!s.is_presentable_as_current());
        let rerun = run(
            &s,
            vec![
                CellEvent::Started {
                    run_id: "r2".into(),
                },
                CellEvent::Completed {
                    output: meta("r2", "b"),
                },
            ],
        );
        assert_eq!(rerun.status, CellStatus::Fresh);
        // An edit before the run started does not taint the new result, but
        // the old output stays stale if the rerun fails.
        let edited = run(
            &rerun,
            vec![
                CellEvent::Invalidated {
                    reason: StaleReason::Cleared,
                },
                CellEvent::Queued,
                CellEvent::Started {
                    run_id: "r3".into(),
                },
            ],
        );
        let failed = run(
            &edited,
            vec![
                CellEvent::Failed { error: msg() },
                CellEvent::ValidationStarted,
                CellEvent::Validated { error: None },
            ],
        );
        assert_eq!(failed.status, CellStatus::Stale);
        let ok = run(
            &edited,
            vec![CellEvent::Completed {
                output: meta("r3", "c"),
            }],
        );
        assert_eq!(ok.status, CellStatus::Fresh);
    }

    #[test]
    fn validation_errors_and_reset() {
        let s = run(
            &CellState::default(),
            vec![
                CellEvent::ValidationStarted,
                CellEvent::Validated { error: Some(msg()) },
            ],
        );
        assert_eq!(s.status, CellStatus::Failed);
        assert_eq!(run(&s, vec![CellEvent::Reset]), CellState::default());
        let s = run(
            &CellState::default(),
            vec![CellEvent::Unavailable { reason: msg() }],
        );
        assert_eq!(s.status, CellStatus::Unavailable);
    }

    fn chain() -> (NotebookDoc, Graph) {
        let mut doc = NotebookDoc::empty("n", "t");
        for (id, src) in [
            ("a", "SELECT 1"),
            ("b", "SELECT * FROM {{a}}"),
            ("c", "SELECT * FROM {{b}}"),
            ("d", "SELECT 2"),
        ] {
            let mut cell = Cell::new(id, CellKind::Sql, Default::default());
            cell.source = src.into();
            doc.cells.push(cell);
        }
        let graph = Graph::build(&doc, &Dialects::default());
        (doc, graph)
    }

    #[test]
    fn failure_blocks_waiting_dependents_only() {
        let (_, graph) = chain();
        let mut nb = NotebookState::default();
        for id in ["a", "b", "c", "d"] {
            nb.apply(&graph, id, CellEvent::Queued).unwrap();
        }
        nb.apply(&graph, "a", CellEvent::Started { run_id: "r".into() })
            .unwrap();
        let changed = nb
            .apply(&graph, "a", CellEvent::Failed { error: msg() })
            .unwrap();
        assert_eq!(changed, vec!["a", "b", "c"]);
        assert_eq!(nb.get("b").status, CellStatus::Blocked);
        assert_eq!(nb.get("b").blocked_by.as_deref(), Some("a"));
        assert_eq!(nb.get("d").status, CellStatus::Queued);
    }

    #[test]
    fn invalidation_propagates_to_dependents_with_outputs() {
        let (_, graph) = chain();
        let mut nb = NotebookState::default();
        for id in ["a", "b", "c"] {
            nb.apply(
                &graph,
                id,
                CellEvent::Started {
                    run_id: format!("r{id}"),
                },
            )
            .unwrap();
            nb.apply(
                &graph,
                id,
                CellEvent::Completed {
                    output: meta(&format!("r{id}"), id),
                },
            )
            .unwrap();
        }
        let changed = nb
            .apply(
                &graph,
                "a",
                CellEvent::Invalidated {
                    reason: StaleReason::Cleared,
                },
            )
            .unwrap();
        assert_eq!(changed, vec!["a", "b", "c"]);
        assert_eq!(
            nb.get("c").stale,
            Some(StaleReason::UpstreamInvalidated { cell: "a".into() })
        );
        assert!(
            ["a", "b", "c"]
                .iter()
                .all(|id| !nb.get(id).is_presentable_as_current())
        );
    }

    #[test]
    fn fingerprint_changes_go_stale_and_reverts_recover() {
        let (_, graph) = chain();
        let mut nb = NotebookState::default();
        nb.apply(&graph, "a", CellEvent::Started { run_id: "r".into() })
            .unwrap();
        nb.apply(
            &graph,
            "a",
            CellEvent::Completed {
                output: meta("r", "a"),
            },
        )
        .unwrap();
        let mut current = BTreeMap::from([("a".to_string(), fp("a2"))]);
        assert_eq!(nb.refresh_staleness(&current), vec!["a"]);
        let a = nb.get("a");
        assert_eq!(a.status, CellStatus::Stale);
        assert_eq!(
            a.stale,
            Some(StaleReason::Changed(vec![
                ChangeReason::Source,
                ChangeReason::KindOrSpec
            ]))
        );
        assert!(nb.refresh_staleness(&current).is_empty(), "idempotent");
        current.insert("a".into(), fp("a"));
        assert_eq!(nb.refresh_staleness(&current), vec!["a"]);
        assert!(nb.get("a").is_presentable_as_current());
    }

    #[test]
    fn ttl_expiry_and_crash_reconciliation() {
        let (mut doc, graph) = chain();
        doc.cells[0].execution_policy.cache = CachePolicy::Ttl { seconds: 60 };
        let mut nb = NotebookState::default();
        nb.apply(&graph, "a", CellEvent::Started { run_id: "r".into() })
            .unwrap();
        nb.apply(
            &graph,
            "a",
            CellEvent::Completed {
                output: meta("r", "a"),
            },
        )
        .unwrap();
        assert!(nb.expire(&doc, 1_000 + 60_000).is_empty());
        assert_eq!(nb.expire(&doc, 1_000 + 60_001), vec!["a"]);
        assert_eq!(nb.get("a").stale, Some(StaleReason::Expired));
        // An expired output does not come back just because fingerprints match.
        nb.refresh_staleness(&BTreeMap::from([("a".to_string(), fp("a"))]));
        assert_eq!(nb.get("a").status, CellStatus::Stale);
        nb.apply(
            &graph,
            "b",
            CellEvent::Started {
                run_id: "rb".into(),
            },
        )
        .unwrap();
        nb.apply(&graph, "c", CellEvent::Queued).unwrap();
        assert_eq!(nb.reconcile_interrupted(), vec!["b", "c"]);
        assert_eq!(nb.get("b").status, CellStatus::Canceled);
        doc.cells.remove(1);
        nb.retain(&doc);
        assert!(!nb.cells.contains_key("b"));
    }

    /// Fixed-seed random event streams: the reducer never panics, never
    /// presents an output whose completeness is partial, and an output is
    /// only presentable when its status is fresh or cached with no stale mark.
    #[test]
    fn randomized_event_streams_keep_invariants() {
        let mut ids = SeededIds(77);
        let mut next = || {
            let mut b = [0u8; 1];
            crate::notebook::document::IdSource::fill(&mut ids, &mut b);
            b[0]
        };
        for _ in 0..300 {
            let mut s = CellState::default();
            let mut run_no = 0;
            for _ in 0..40 {
                let event = match next() % 13 {
                    0 => CellEvent::ValidationStarted,
                    1 => CellEvent::Validated {
                        error: (next() % 2 == 0).then(msg),
                    },
                    2 => CellEvent::Waiting,
                    3 => CellEvent::Queued,
                    4 => {
                        run_no += 1;
                        CellEvent::Started {
                            run_id: format!("r{run_no}"),
                        }
                    }
                    5 => CellEvent::Streaming {
                        rows: next() as u64,
                    },
                    6 | 7 => {
                        let mut m = meta(&format!("r{run_no}"), "x");
                        m.completeness = if next() % 3 == 0 {
                            Completeness::Partial
                        } else {
                            Completeness::Complete
                        };
                        m.from_cache = next() % 4 == 0;
                        CellEvent::Completed { output: m }
                    }
                    8 => CellEvent::Failed { error: msg() },
                    9 => CellEvent::Canceled {
                        server_confirmed: next() % 2 == 0,
                    },
                    10 => CellEvent::Invalidated {
                        reason: StaleReason::Cleared,
                    },
                    11 => CellEvent::Blocked { by: "z".into() },
                    _ => CellEvent::AwaitingApproval,
                };
                if let Ok(n) = s.reduce(event) {
                    s = n;
                }
                if s.is_presentable_as_current() {
                    assert!(matches!(s.status, CellStatus::Fresh | CellStatus::Cached));
                    assert!(s.stale.is_none());
                    assert_ne!(
                        s.output.as_ref().unwrap().completeness,
                        Completeness::Partial
                    );
                }
                if s.status == CellStatus::Stale {
                    assert!(s.stale.is_some());
                }
            }
        }
    }
}
