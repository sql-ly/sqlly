//! Explain changes: what changed for each cell between a baseline (the
//! outputs an earlier AI run read, or a recorded run) and now, attributed
//! from F1's fingerprint parts (SQL, settings, connection, policy,
//! parameters, an input cell) and, when the fingerprint is the same, from
//! the data itself (row counts, checksums and, with both outputs, the
//! analytics result diff).

use std::collections::BTreeMap;

use sqlly_core::StoredMessage;

use super::context::{NotebookContext, observed, view_completeness};
use crate::analytics::OutputView;
use crate::analytics::diff::{DiffOptions, diff_outputs};
use crate::notebook::CellOutput;
use crate::notebook::cells::ai::{
    Evidence, Finding, FindingKind, FindingOrigin, ObservedOutput, ReviewCheck,
};
use crate::notebook::fingerprint::ChangeReason;

fn reason_key(reason: ChangeReason) -> &'static str {
    match reason {
        ChangeReason::Source => "notebook.ai.explain.source",
        ChangeReason::KindOrSpec => "notebook.ai.explain.kind_or_spec",
        ChangeReason::Connection => "notebook.ai.explain.connection",
        ChangeReason::Policy => "notebook.ai.explain.policy",
        ChangeReason::Parameters => "notebook.ai.explain.parameters",
        ChangeReason::Upstream => "notebook.ai.explain.upstream",
    }
}

fn reason_english(reason: ChangeReason) -> &'static str {
    match reason {
        ChangeReason::Source => "its SQL changed",
        ChangeReason::KindOrSpec => "its settings changed",
        ChangeReason::Connection => "it ran on a different connection or sign-in",
        ChangeReason::Policy => "its safety or cache settings changed",
        ChangeReason::Parameters => "the parameters it uses changed",
        ChangeReason::Upstream => "a cell it reads from changed",
    }
}

/// The fingerprint parts that differ between two observations.
pub fn changed_parts(before: &ObservedOutput, after: &ObservedOutput) -> Vec<ChangeReason> {
    match (&before.parts, &after.parts) {
        (Some(old), Some(new)) => {
            let reasons = [
                (old.source != new.source, ChangeReason::Source),
                (old.kind_spec != new.kind_spec, ChangeReason::KindOrSpec),
                (old.connection != new.connection, ChangeReason::Connection),
                (old.policy != new.policy, ChangeReason::Policy),
                (old.params != new.params, ChangeReason::Parameters),
                (old.upstream != new.upstream, ChangeReason::Upstream),
            ];
            reasons
                .into_iter()
                .filter_map(|(changed, reason)| changed.then_some(reason))
                .collect()
        }
        _ if before.fingerprint != after.fingerprint => vec![ChangeReason::Source],
        _ => vec![],
    }
}

fn finding(
    id: usize,
    kind: FindingKind,
    key: &str,
    english: String,
    arguments: Vec<(String, String)>,
    cell: &str,
    runs: [&str; 2],
) -> Finding {
    let mut finding = Finding::new(format!("x{id}"), kind, english);
    finding.message = Some(StoredMessage {
        key: key.to_string(),
        arguments,
    });
    finding.check = Some(ReviewCheck::ChangeExplained);
    finding.origin = FindingOrigin::Deterministic;
    for run in runs.into_iter().filter(|r| !r.is_empty()) {
        finding.evidence.push(Evidence {
            cell: Some(cell.to_string()),
            run: Some(run.to_string()),
            ..Evidence::default()
        });
    }
    finding.verified = true;
    finding
}

/// Explain one cell's change from `before` to `after`. With both full
/// outputs the data difference is counted row by row.
pub fn explain_cell(
    cell: &str,
    label: &str,
    before: &ObservedOutput,
    after: &ObservedOutput,
    outputs: Option<(&CellOutput, &CellOutput)>,
    next_id: &mut usize,
) -> Vec<Finding> {
    let mut out = vec![];
    let runs = [before.run_id.as_str(), after.run_id.as_str()];
    let mut push = |out: &mut Vec<Finding>,
                    kind,
                    key: &str,
                    english: String,
                    arguments: Vec<(String, String)>| {
        *next_id += 1;
        out.push(finding(*next_id, kind, key, english, arguments, cell, runs));
    };
    let reasons = changed_parts(before, after);
    for reason in &reasons {
        push(
            &mut out,
            FindingKind::ObservedFact,
            reason_key(*reason),
            format!("{label} changed because {}.", reason_english(*reason)),
            vec![("cell".into(), label.to_string())],
        );
    }
    let added: Vec<&String> = after
        .columns
        .iter()
        .filter(|c| !before.columns.contains(c))
        .collect();
    let removed: Vec<&String> = before
        .columns
        .iter()
        .filter(|c| !after.columns.contains(c))
        .collect();
    if !added.is_empty() || !removed.is_empty() {
        let list = |v: &[&String]| {
            if v.is_empty() {
                "none".to_string()
            } else {
                v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            }
        };
        push(
            &mut out,
            FindingKind::ObservedFact,
            "notebook.ai.explain.columns",
            format!(
                "{label}'s columns changed: added {}, removed {}.",
                list(&added),
                list(&removed)
            ),
            vec![
                ("cell".into(), label.to_string()),
                ("added".into(), list(&added)),
                ("removed".into(), list(&removed)),
            ],
        );
    }
    if before.checksum == after.checksum && before.row_count == after.row_count {
        if reasons.is_empty() {
            push(
                &mut out,
                FindingKind::ObservedFact,
                "notebook.ai.explain.unchanged",
                format!(
                    "{label} returned the same {} rows as before.",
                    after.row_count
                ),
                vec![
                    ("cell".into(), label.to_string()),
                    ("rows".into(), after.row_count.to_string()),
                ],
            );
        }
        return out;
    }
    let detail = outputs.and_then(|(old, new)| {
        diff_outputs(
            &OutputView::new(&old.columns, &old.rows, view_completeness(old)),
            &OutputView::new(&new.columns, &new.rows, view_completeness(new)),
            &DiffOptions::default(),
        )
        .ok()
    });
    let (key, english, mut arguments) = match &detail {
        Some(diff) => (
            "notebook.ai.explain.rows_diff",
            format!(
                "{label}'s data changed: {} rows added, {} removed, {} changed ({} before, {} now).",
                diff.summary.added,
                diff.summary.removed,
                diff.summary.changed,
                before.row_count,
                after.row_count
            ),
            vec![
                ("added".to_string(), diff.summary.added.to_string()),
                ("removed".to_string(), diff.summary.removed.to_string()),
                ("changed".to_string(), diff.summary.changed.to_string()),
            ],
        ),
        None if reasons.is_empty() => (
            "notebook.ai.explain.data_same_inputs",
            format!(
                "{label}'s data changed: {} rows before, {} now, with the same SQL, settings, connection and parameters.",
                before.row_count, after.row_count
            ),
            vec![],
        ),
        None => (
            "notebook.ai.explain.data",
            format!(
                "{label}'s data changed: {} rows before, {} now.",
                before.row_count, after.row_count
            ),
            vec![],
        ),
    };
    arguments.extend([
        ("cell".to_string(), label.to_string()),
        ("before".to_string(), before.row_count.to_string()),
        ("after".to_string(), after.row_count.to_string()),
    ]);
    let kind = if before.complete && after.complete {
        FindingKind::ObservedFact
    } else {
        FindingKind::Hypothesis
    };
    push(&mut out, kind, key, english, arguments);
    out
}

/// Explain every cell the baseline observed against the notebook as it is
/// now. Cells without a current result are reported as not comparable.
pub fn explain_since(
    ctx: &NotebookContext,
    baseline: &BTreeMap<String, ObservedOutput>,
) -> Vec<Finding> {
    let mut next_id = 0usize;
    let mut out = vec![];
    if baseline.is_empty() {
        let mut f = Finding::new(
            "x1",
            FindingKind::Hypothesis,
            "There is no earlier AI run to compare with; run this cell once to record a baseline.",
        );
        f.message = Some(StoredMessage {
            key: "notebook.ai.explain.no_baseline".into(),
            arguments: vec![],
        });
        f.check = Some(ReviewCheck::ChangeExplained);
        f.origin = FindingOrigin::Deterministic;
        return vec![f];
    }
    for (cell, before) in baseline {
        let label = ctx
            .doc
            .cell(cell)
            .map(|c| c.relation_name().to_string())
            .unwrap_or_else(|| cell.clone());
        let Some(output) = ctx.outputs.get(cell) else {
            next_id += 1;
            out.push(finding(
                next_id,
                FindingKind::Hypothesis,
                "notebook.ai.explain.not_run",
                format!("{label} has no result now, so it cannot be compared."),
                vec![("cell".into(), label.clone())],
                cell,
                [before.run_id.as_str(), ""],
            ));
            continue;
        };
        let parts = ctx.runtime.get(cell).and_then(|r| r.parts.clone());
        let after = observed(output, parts);
        out.extend(explain_cell(
            cell,
            &label,
            before,
            &after,
            None,
            &mut next_id,
        ));
    }
    out
}

/// A recorded run as one side of Explain changes: the outputs it kept and
/// the fingerprint parts it recorded, per cell.
#[derive(Debug, Clone, Default)]
pub struct RecordedRun {
    pub run_id: String,
    pub outputs: BTreeMap<String, CellOutput>,
    pub parts: BTreeMap<String, crate::notebook::fingerprint::FingerprintParts>,
}

/// Explain what changed for each cell `before` kept, against the run
/// `after` or, with `None`, against the notebook as it is now. Both full
/// outputs are at hand, so data changes are counted row by row.
pub fn explain_runs(
    ctx: &NotebookContext,
    before: &RecordedRun,
    after: Option<&RecordedRun>,
) -> Vec<Finding> {
    let mut next_id = 0usize;
    if before.outputs.is_empty() {
        next_id += 1;
        return vec![finding(
            next_id,
            FindingKind::Hypothesis,
            "notebook.ai.explain.no_run_outputs",
            "The chosen run kept no results to compare with.".to_string(),
            vec![],
            "",
            [before.run_id.as_str(), ""],
        )];
    }
    let mut out = vec![];
    for (cell, old) in &before.outputs {
        let label = ctx
            .doc
            .cell(cell)
            .map(|c| c.relation_name().to_string())
            .unwrap_or_else(|| cell.clone());
        let (new, parts) = match after {
            Some(run) => (run.outputs.get(cell), run.parts.get(cell).cloned()),
            None => (
                ctx.outputs.get(cell),
                ctx.runtime.get(cell).and_then(|r| r.parts.clone()),
            ),
        };
        let Some(new) = new else {
            next_id += 1;
            out.push(finding(
                next_id,
                FindingKind::Hypothesis,
                "notebook.ai.explain.not_run",
                format!("{label} has no result now, so it cannot be compared."),
                vec![("cell".into(), label.clone())],
                cell,
                [before.run_id.as_str(), ""],
            ));
            continue;
        };
        let was = observed(old, before.parts.get(cell).cloned());
        let now = observed(new, parts);
        out.extend(explain_cell(
            cell,
            &label,
            &was,
            &now,
            Some((old, new)),
            &mut next_id,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::fingerprint::{Fingerprint, FingerprintParts};
    use crate::notebook::{Completeness, Provenance};
    use sqlly_core::{CellValue, ResultColumn};
    use std::sync::Arc;

    fn parts(params: &str) -> FingerprintParts {
        let f = |s: &str| Fingerprint(s.to_string());
        FingerprintParts {
            source: f("s"),
            kind_spec: f("k"),
            connection: f("c"),
            policy: f("p"),
            params: f(params),
            upstream: f("u"),
        }
    }

    fn out(rows: Vec<i64>, run: &str) -> CellOutput {
        CellOutput {
            columns: vec![ResultColumn {
                name: "n".into(),
                data_type: "int".into(),
                nullable: false,
            }],
            row_count: rows.len() as u64,
            rows: Arc::new(rows.into_iter().map(|n| vec![CellValue::I64(n)]).collect()),
            completeness: Completeness::Complete,
            elapsed_ms: 1,
            run_id: run.into(),
            fingerprint: Fingerprint("fp".into()),
            provenance: Provenance::default(),
            freshness: Default::default(),
        }
    }

    #[test]
    fn parameter_changes_are_attributed_before_data() {
        let old = out(vec![1, 2], "r1");
        let new = out(vec![1, 2, 3], "r2");
        let before = observed(&old, Some(parts("eu")));
        let after = observed(&new, Some(parts("us")));
        let mut id = 0;
        let findings = explain_cell("c1", "orders", &before, &after, Some((&old, &new)), &mut id);
        assert_eq!(
            findings[0].message.as_ref().unwrap().key,
            "notebook.ai.explain.parameters"
        );
        assert_eq!(
            findings[1].message.as_ref().unwrap().key,
            "notebook.ai.explain.rows_diff"
        );
        assert!(findings[1].text.contains("1 rows added, 0 removed"));
        assert_eq!(findings[1].evidence.len(), 2);
        assert_eq!(findings[1].evidence[1].run.as_deref(), Some("r2"));
    }

    #[test]
    fn same_inputs_with_new_rows_mean_the_data_changed() {
        let before = observed(&out(vec![1], "r1"), Some(parts("x")));
        let after = observed(&out(vec![1, 5], "r2"), Some(parts("x")));
        let mut id = 0;
        let findings = explain_cell("c1", "orders", &before, &after, None, &mut id);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .text
                .contains("with the same SQL, settings, connection and parameters")
        );
        let same = explain_cell("c1", "orders", &before, &before.clone(), None, &mut id);
        assert_eq!(
            same[0].message.as_ref().unwrap().key,
            "notebook.ai.explain.unchanged"
        );
    }

    #[test]
    fn recorded_runs_compare_with_each_other_and_with_now() {
        use crate::notebook::NotebookDoc;
        let doc: NotebookDoc = serde_json::from_value(serde_json::json!({
            "format": "sqlly.notebook", "format_version": 2, "notebook_id": "nb", "title": "T",
            "cells": [{"id": "c1", "name": "orders", "kind": "sql", "source": "SELECT 1"}]
        }))
        .unwrap();
        let run = |id: &str, rows: Vec<i64>, params: &str| RecordedRun {
            run_id: id.into(),
            outputs: [("c1".to_string(), out(rows, id))].into(),
            parts: [("c1".to_string(), parts(params))].into(),
        };
        let mut ctx = NotebookContext {
            doc,
            outputs: BTreeMap::new(),
            runtime: BTreeMap::new(),
            trust: crate::notebook_security::TrustDecision::untrusted(&NotebookDoc::empty(
                "nb", "T",
            )),
            connections: BTreeMap::new(),
            ai_cell: "ai".into(),
        };
        let first = run("r1", vec![1, 2], "eu");
        let second = run("r2", vec![1, 2, 3], "us");
        let between = explain_runs(&ctx, &first, Some(&second));
        let keys: Vec<&str> = between
            .iter()
            .map(|f| f.message.as_ref().unwrap().key.as_str())
            .collect();
        assert_eq!(
            keys,
            vec![
                "notebook.ai.explain.parameters",
                "notebook.ai.explain.rows_diff"
            ]
        );
        assert!(between[1].text.contains("1 rows added"));
        assert_eq!(between[1].evidence[0].run.as_deref(), Some("r1"));
        assert_eq!(between[1].evidence[1].run.as_deref(), Some("r2"));

        // Against now: the cell has not run yet, then it has the same rows.
        let missing = explain_runs(&ctx, &first, None);
        assert_eq!(
            missing[0].message.as_ref().unwrap().key,
            "notebook.ai.explain.not_run"
        );
        ctx.outputs.insert("c1".into(), out(vec![1, 2], "r_now"));
        ctx.runtime.insert(
            "c1".into(),
            crate::notebook_agent::context::CellRuntime {
                parts: Some(parts("eu")),
                ..Default::default()
            },
        );
        let same = explain_runs(&ctx, &first, None);
        assert_eq!(
            same[0].message.as_ref().unwrap().key,
            "notebook.ai.explain.unchanged"
        );
        let empty = explain_runs(&ctx, &RecordedRun::default(), None);
        assert_eq!(
            empty[0].message.as_ref().unwrap().key,
            "notebook.ai.explain.no_run_outputs"
        );
    }
}
