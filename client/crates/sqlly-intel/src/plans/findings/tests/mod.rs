//! Tests for the finding rules.

mod access;
mod engines;
mod estimates;
mod join;
mod memory;
mod misc;
mod pipeline;
mod predicates;
mod schema;

use super::*;
use crate::plans::import::import_plan_document;
use crate::plans::plan_fixtures::{est, op, plan, ran, scan};
use crate::plans::{ExecutionPlan, PlanDialect, PlanNode};
use FindingConfidence as C;
use FindingKind as K;
use FindingSeverity as S;

/// The one finding of `kind`, or a panic that prints everything found.
fn get(findings: &[PlanFinding], kind: K) -> &PlanFinding {
    let mut matching = findings.iter().filter(|finding| finding.kind == kind);
    let first = matching
        .next()
        .unwrap_or_else(|| panic!("no {} finding in:\n{}", kind.as_str(), render(findings)));
    assert!(
        matching.next().is_none(),
        "more than one {} finding in:\n{}",
        kind.as_str(),
        render(findings)
    );
    first
}

fn has(findings: &[PlanFinding], kind: K) -> bool {
    findings.iter().any(|finding| finding.kind == kind)
}

fn assert_none(findings: &[PlanFinding], kind: K) {
    assert!(
        !has(findings, kind),
        "unexpected {} finding in:\n{}",
        kind.as_str(),
        render(findings)
    );
}

/// The variant of the rule that fired: the part of the summary key between
/// the kind and `.summary`.
fn variant(finding: &PlanFinding) -> String {
    let key = finding.summary.key().unwrap_or_default();
    let prefix = format!("services.plans.findings.{}.", finding.kind.as_str());
    key.strip_prefix(prefix.as_str())
        .and_then(|rest| rest.strip_suffix(".summary"))
        .unwrap_or(key)
        .to_string()
}

fn sql(finding: &PlanFinding) -> &str {
    finding.action_sql.as_deref().unwrap_or("")
}

fn has_note(finding: &PlanFinding, short: &str) -> bool {
    let key = format!("services.plans.findings.{short}");
    finding
        .notes
        .iter()
        .any(|note| note.key() == Some(key.as_str()))
}

fn evidence(finding: &PlanFinding) -> String {
    finding
        .evidence
        .iter()
        .map(|line| line.fallback().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A statement that ran for `ms` in all.
fn timed(mut plan: ExecutionPlan, ms: f64) -> ExecutionPlan {
    plan.execution_time_ms = Some(ms);
    plan
}

fn node(id: i32, name: &str, rows: f64, loops: f64, time_ms: f64) -> PlanNode {
    ran(op(id, name), rows, loops, time_ms)
}

fn read(id: i32, name: &str, table: &str, rows: f64, loops: f64, time_ms: f64) -> PlanNode {
    ran(scan(id, name, table), rows, loops, time_ms)
}

fn sample(name: &str) -> Vec<ExecutionPlan> {
    // The online viewer's samples, or this crate's own copy of a plan the
    // viewer no longer ships.
    let own = format!("{}/tests/plan_samples/{name}", env!("CARGO_MANIFEST_DIR"));
    let path = if std::path::Path::new(&own).exists() {
        own
    } else {
        format!(
            "{}/../../apps/planview-wasm/samples/{name}",
            env!("CARGO_MANIFEST_DIR")
        )
    };
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    import_plan_document(&text)
        .unwrap_or_else(|error| panic!("{name}: {error:?}"))
        .plans
}

pub(super) fn render(findings: &[PlanFinding]) -> String {
    let mut out = String::new();
    for finding in findings {
        out.push_str(&format!(
            "[{} | {:?} | {:?} | rank {:.1} | nodes {:?}]\n",
            finding.kind.as_str(),
            finding.severity,
            finding.confidence,
            finding.rank,
            finding.node_ids
        ));
        if let Some(impact) = finding.impact_message() {
            out.push_str(&format!("  impact:  {}\n", impact.fallback()));
        }
        out.push_str(&format!("  summary: {}\n", finding.summary.fallback()));
        out.push_str(&format!("  why:     {}\n", finding.why.fallback()));
        out.push_str(&format!("  action:  {}\n", finding.action.fallback()));
        if let Some(sql) = &finding.action_sql {
            out.push_str(&format!(
                "  sql:     {}\n",
                sql.replace('\n', "\n           ")
            ));
        }
        for alternative in &finding.alternatives {
            out.push_str(&format!("  or:      {}\n", alternative.label.fallback()));
            if let Some(sql) = &alternative.sql {
                out.push_str(&format!(
                    "           {}\n",
                    sql.replace('\n', "\n           ")
                ));
            }
        }
        for note in &finding.notes {
            out.push_str(&format!("  note:    {}\n", note.fallback()));
        }
        for evidence in &finding.evidence {
            out.push_str(&format!("  - {}\n", evidence.fallback()));
        }
    }
    out
}

/// Prints the findings for every sample plan the browser viewer ships.
/// `cargo test -p sqlly-intel --lib findings::tests::dump_samples -- --ignored --nocapture`
#[test]
#[ignore = "prints the findings for the sample plans"]
fn dump_samples() {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../apps/planview-wasm/samples"
    );
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    names.sort();
    for path in names {
        let text = std::fs::read_to_string(&path).unwrap();
        println!("==== {}", path.file_name().unwrap().to_string_lossy());
        match import_plan_document(&text) {
            Ok(imported) => {
                for (index, plan) in imported.plans.iter().enumerate() {
                    println!("-- statement {} ({:?})", index + 1, plan.dialect);
                    let findings = findings_for_plan(plan);
                    if findings.is_empty() {
                        println!("(no findings)");
                    }
                    print!("{}", render(&findings));
                }
            }
            Err(error) => println!("import failed: {error:?}"),
        }
    }
}
