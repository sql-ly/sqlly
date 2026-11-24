//! Reading the model's final answer and deciding, in Rust, which of its
//! claims rest on executed evidence. The model labels its findings; SQLLY
//! checks the links: a cited cell must have a result the agent read in
//! this run, a cited query must have run, a quoted value must appear in the
//! evidence, and a claim over sampled or cut-short data is not verified.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::context::{NotebookContext, checksum};
use crate::notebook::cells::ai::{
    Evidence, Finding, FindingKind, InvestigationPlan, PlanStep, UnverifiedReason,
};

/// The parts of a final answer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedAnswer {
    pub prose: String,
    pub findings: Vec<RawFinding>,
    pub plan: Option<InvestigationPlan>,
    pub clarifications: Vec<String>,
}

/// A finding as the model wrote it, before verification.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFinding {
    pub kind: FindingKind,
    pub text: String,
    pub evidence: Vec<RawEvidence>,
    pub metric: Option<String>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvidence {
    pub cell: Option<String>,
    pub query: Option<String>,
    pub group: Option<String>,
}

fn blocks(text: &str) -> (String, Vec<(String, String)>) {
    let mut prose = String::new();
    let mut found = vec![];
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(newline) = after.find('\n') else {
            break;
        };
        let tag = after[..newline].trim().to_ascii_lowercase();
        let body = &after[newline + 1..];
        let Some(end) = body.find("```") else {
            break;
        };
        prose.push_str(&rest[..start]);
        found.push((tag, body[..end].trim().to_string()));
        rest = &body[end + 3..];
    }
    prose.push_str(rest);
    (prose.trim().to_string(), found)
}

fn strings(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.trim().to_string()),
                Value::Object(map) => map
                    .get("question")
                    .or_else(|| map.get("text"))
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect(),
        Value::String(s) if !s.trim().is_empty() => vec![s.trim().to_string()],
        _ => vec![],
    }
}

fn evidence_item(value: &Value) -> Option<RawEvidence> {
    match value {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            let is_query =
                s.len() > 1 && s.starts_with('q') && s[1..].chars().all(|c| c.is_ascii_digit());
            Some(if is_query {
                RawEvidence {
                    cell: None,
                    query: Some(s.to_string()),
                    group: None,
                }
            } else {
                RawEvidence {
                    cell: Some(s.to_string()),
                    query: None,
                    group: None,
                }
            })
        }
        Value::Object(map) => {
            let get = |k: &str| {
                map.get(k)
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.trim().to_string()),
                        Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    })
                    .filter(|s| !s.is_empty())
            };
            let evidence = RawEvidence {
                cell: get("cell"),
                query: get("query"),
                group: get("group").or_else(|| get("aggregate")),
            };
            (evidence.cell.is_some() || evidence.query.is_some()).then_some(evidence)
        }
        _ => None,
    }
}

fn finding(value: &Value) -> Option<RawFinding> {
    let map = value.as_object()?;
    let text = map
        .get("text")
        .or_else(|| map.get("statement"))
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    if text.is_empty() {
        return None;
    }
    let kind = map
        .get("kind")
        .or_else(|| map.get("type"))
        .and_then(Value::as_str)
        .and_then(FindingKind::parse)
        .unwrap_or(FindingKind::Hypothesis);
    let evidence = match map.get("evidence") {
        Some(Value::Array(items)) => items.iter().filter_map(evidence_item).collect(),
        Some(other) => evidence_item(other).into_iter().collect(),
        None => vec![],
    };
    let scalar = |k: &str| {
        map.get(k).and_then(|v| match v {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    };
    Some(RawFinding {
        kind,
        text,
        evidence,
        metric: scalar("metric"),
        value: scalar("value"),
    })
}

fn plan(value: &Value) -> Option<InvestigationPlan> {
    let map = value.as_object()?;
    let steps = match map.get("steps") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(s) => Some(PlanStep {
                    title: s.trim().to_string(),
                    ..PlanStep::default()
                }),
                Value::Object(step) => {
                    let title = step
                        .get("title")
                        .or_else(|| step.get("step"))
                        .and_then(Value::as_str)?
                        .trim()
                        .to_string();
                    Some(PlanStep {
                        title,
                        detail: step
                            .get("detail")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                        cells: step.get("cells").map(strings).unwrap_or_default(),
                    })
                }
                _ => None,
            })
            .filter(|s| !s.title.is_empty())
            .collect(),
        _ => vec![],
    };
    let list = |k: &str| map.get(k).map(strings).unwrap_or_default();
    let plan = InvestigationPlan {
        goal: map
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        steps,
        sources: list("sources"),
        assumptions: list("assumptions"),
        concerns: list("concerns"),
        permissions: list("permissions"),
        approved: false,
    };
    (!plan.steps.is_empty() || !plan.goal.is_empty()).then_some(plan)
}

/// Split a final answer into prose and its fenced `findings`, `plan` and
/// `clarify` blocks (a `json` block with those keys works too).
pub fn parse_answer(text: &str) -> ParsedAnswer {
    let (prose, found) = blocks(text);
    let mut answer = ParsedAnswer {
        prose,
        ..ParsedAnswer::default()
    };
    for (tag, body) in found {
        let Ok(value) = serde_json::from_str::<Value>(&body) else {
            if tag == "sql" || tag.is_empty() {
                answer.prose.push_str(&format!("\n\n```{tag}\n{body}\n```"));
            }
            continue;
        };
        let mut take = |key: &str, value: &Value| match key {
            "findings" => answer
                .findings
                .extend(value.as_array().into_iter().flatten().filter_map(finding)),
            "plan" => answer.plan = plan(value).or(answer.plan.take()),
            "clarify" | "clarifications" | "questions" => {
                answer.clarifications.extend(strings(value))
            }
            _ => {}
        };
        match tag.as_str() {
            "findings" | "plan" | "clarify" | "clarifications" => take(&tag, &value),
            _ => {
                if let Some(map) = value.as_object() {
                    for key in ["findings", "plan", "clarifications", "clarify", "questions"] {
                        if let Some(v) = map.get(key) {
                            take(key, v);
                        }
                    }
                }
            }
        }
    }
    answer
}

/// A query the agent ran, as evidence.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryEvidence {
    pub ok: bool,
    /// The result text the model saw (for value checks).
    pub text: String,
    pub result_hash: Option<String>,
    /// The row limit cut the result short.
    pub cut_short: bool,
}

/// What a run executed and read.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunEvidence {
    pub queries: BTreeMap<String, QueryEvidence>,
    /// Cells whose output the agent read.
    pub read_outputs: BTreeSet<String>,
}

impl RunEvidence {
    pub fn executed(&self) -> bool {
        self.queries.values().any(|q| q.ok) || !self.read_outputs.is_empty()
    }
}

fn normalize_number(text: &str) -> Option<f64> {
    let cleaned: String = text
        .trim()
        .trim_start_matches(['$', '€', '£'])
        .trim_end_matches('%')
        .chars()
        .filter(|c| !matches!(c, ',' | '_' | ' ' | '\u{a0}'))
        .collect();
    cleaned.parse::<f64>().ok().filter(|f| f.is_finite())
}

fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '|' | ',' | ';' | '(' | ')'))
        .filter(|t| !t.is_empty())
}

/// Whether `value` appears in `haystack`: numerically when both are numbers
/// (so `1,204` matches `1204.00`), else as case-insensitive text.
pub fn value_in(value: &str, haystack: &str) -> bool {
    match normalize_number(value) {
        Some(wanted) => {
            let tolerance = (wanted.abs() * 1e-9).max(1e-9);
            // A thousands separator splits a number into tokens, so the
            // text is also read with commas removed.
            tokens(haystack)
                .chain(tokens(&haystack.replace(',', "")))
                .filter_map(normalize_number)
                .any(|found| (found - wanted).abs() <= tolerance)
        }
        None => haystack
            .to_lowercase()
            .contains(&value.trim().to_lowercase()),
    }
}

fn output_text(ctx: &NotebookContext, id: &str) -> String {
    let Some(output) = ctx.outputs.get(id) else {
        return String::new();
    };
    let mut text = String::new();
    for row in super::context::masked_rows(output, output.rows.len()) {
        for value in row {
            text.push_str(&match value {
                sqlly_core::CellValue::Null => "NULL".to_string(),
                sqlly_core::CellValue::Bool(b) => b.to_string(),
                sqlly_core::CellValue::I64(n) => n.to_string(),
                sqlly_core::CellValue::F64(f) => f.to_string(),
                sqlly_core::CellValue::String(s) => s,
                sqlly_core::CellValue::Bytes(_) => String::new(),
            });
            text.push(' ');
        }
        text.push('\n');
    }
    text.push_str(&format!("{} ", output.row_count));
    text
}

/// Verify raw findings against what the run executed. Ids are `f1`, `f2`, …
/// after `first_id - 1`.
pub fn verify(
    raw: &[RawFinding],
    ctx: &NotebookContext,
    evidence: &RunEvidence,
    first_id: usize,
) -> Vec<Finding> {
    let executed = evidence.executed();
    let mut out: Vec<Finding> = raw
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let mut finding =
                Finding::new(format!("f{}", first_id + i), raw.kind, raw.text.clone());
            finding.metric = raw.metric.clone();
            finding.value = raw.value.clone();
            let mut problem: Option<UnverifiedReason> = None;
            let mut texts: Vec<String> = vec![];
            for item in &raw.evidence {
                let mut link = Evidence {
                    group: item.group.clone(),
                    ..Evidence::default()
                };
                if let Some(q) = &item.query {
                    link.query = Some(q.clone());
                    match evidence.queries.get(q) {
                        Some(query) if query.ok => {
                            link.result_hash = query.result_hash.clone();
                            texts.push(query.text.clone());
                            if query.cut_short && raw.kind.is_claim() {
                                problem.get_or_insert(UnverifiedReason::IncompleteEvidence);
                            }
                        }
                        _ => {
                            problem.get_or_insert(UnverifiedReason::UnknownEvidence);
                        }
                    }
                }
                if let Some(token) = &item.cell {
                    match ctx.resolve_cell(token) {
                        Some(cell) => {
                            link.cell = Some(cell.id.clone());
                            match ctx.outputs.get(&cell.id) {
                                Some(output) if evidence.read_outputs.contains(&cell.id) => {
                                    link.run = Some(output.run_id.clone());
                                    link.result_hash = Some(checksum(output));
                                    texts.push(output_text(ctx, &cell.id));
                                    if !output.completeness.supports_summaries()
                                        && raw.kind.is_claim()
                                    {
                                        problem.get_or_insert(UnverifiedReason::IncompleteEvidence);
                                    }
                                }
                                _ => {
                                    problem.get_or_insert(UnverifiedReason::UnknownEvidence);
                                }
                            }
                        }
                        None => {
                            link.cell = Some(token.clone());
                            problem.get_or_insert(UnverifiedReason::UnknownEvidence);
                        }
                    }
                }
                finding.evidence.push(link);
            }
            if !executed {
                problem = Some(UnverifiedReason::NotExecuted);
            } else if finding.evidence.is_empty() {
                problem.get_or_insert(UnverifiedReason::NoEvidence);
            }
            if problem.is_none()
                && raw.kind == FindingKind::ObservedFact
                && let Some(value) = &raw.value
                && !texts.iter().any(|t| value_in(value, t))
            {
                problem = Some(UnverifiedReason::ValueNotInEvidence);
            }
            finding.verified = problem.is_none();
            finding.unverified_reason = problem;
            finding
        })
        .collect();
    mark_conflicts(&mut out);
    out
}

/// Claims that state different values for the same metric point at each
/// other; neither is dropped, so the user sees the contradiction.
pub fn mark_conflicts(findings: &mut [Finding]) {
    let key = |f: &Finding| -> Option<(String, String)> {
        if !f.kind.is_claim() {
            return None;
        }
        let metric = f.metric.as_deref()?.trim().to_lowercase();
        let value = f.value.as_deref()?.trim();
        let value = normalize_number(value)
            .map(|n| format!("{n}"))
            .unwrap_or_else(|| value.to_lowercase());
        Some((metric, value))
    };
    let keys: Vec<Option<(String, String)>> = findings.iter().map(key).collect();
    for i in 0..findings.len() {
        for j in 0..findings.len() {
            if i == j {
                continue;
            }
            if let (Some((mi, vi)), Some((mj, vj))) = (&keys[i], &keys[j])
                && mi == mj
                && vi != vj
            {
                let other = findings[j].id.clone();
                if !findings[i].conflicts_with.contains(&other) {
                    findings[i].conflicts_with.push(other);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::fingerprint::Fingerprint;
    use crate::notebook::{
        Cell, CellKind, CellOutput, Completeness, ExecutionPolicy, NotebookDoc, Provenance,
        SeededIds,
    };
    use crate::notebook_security::TrustDecision;
    use sqlly_core::{CellValue, ResultColumn};
    use std::sync::Arc;

    fn ctx(completeness: Completeness) -> NotebookContext {
        let mut doc = NotebookDoc::new("t", &mut SeededIds(5));
        doc.cells.clear();
        let mut cell = Cell::new("c_refunds", CellKind::Sql, ExecutionPolicy::default());
        cell.name = Some("refunds".into());
        doc.cells.push(cell);
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "c_refunds".to_string(),
            CellOutput {
                columns: vec![ResultColumn {
                    name: "total".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
                rows: Arc::new(vec![vec![CellValue::I64(1204)]]),
                completeness,
                row_count: 1,
                elapsed_ms: 1,
                run_id: "run_7".into(),
                fingerprint: Fingerprint("f".into()),
                provenance: Provenance::default(),
                freshness: Default::default(),
            },
        );
        NotebookContext {
            trust: TrustDecision::untrusted(&doc),
            doc,
            outputs,
            runtime: BTreeMap::new(),
            connections: BTreeMap::new(),
            ai_cell: "c_ai".into(),
        }
    }

    const ANSWER: &str = r#"Refunds doubled in the EU.

```findings
[
 {"kind": "observed_fact", "text": "EU refunds were 1,204", "evidence": [{"cell": "refunds"}], "metric": "eu_refunds", "value": "1,204"},
 {"kind": "observed_fact", "text": "EU refunds were 980", "evidence": ["q1"], "metric": "eu_refunds", "value": "980"},
 {"kind": "derived_metric", "text": "Refund rate 4.1%", "evidence": ["q1", {"cell": "refunds", "group": "region = EU"}]},
 {"kind": "hypothesis", "text": "A payment provider change"},
 {"kind": "recommendation", "text": "Check the settlement cell", "evidence": [{"cell": "nope"}]}
]
```

```clarify
["Do you mean gross or net refunds?"]
```"#;

    #[test]
    fn answers_split_into_prose_findings_and_questions() {
        let answer = parse_answer(ANSWER);
        assert_eq!(answer.prose, "Refunds doubled in the EU.");
        assert_eq!(answer.findings.len(), 5);
        assert_eq!(answer.findings[1].evidence[0].query.as_deref(), Some("q1"));
        assert_eq!(
            answer.clarifications,
            vec!["Do you mean gross or net refunds?"]
        );
        let plan = parse_answer(
            "```json\n{\"plan\": {\"goal\": \"g\", \"steps\": [\"a\", {\"title\": \"b\", \"cells\": [\"c1\"]}], \"concerns\": [\"cost\"]}}\n```",
        )
        .plan
        .unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[1].cells, vec!["c1"]);
        assert_eq!(plan.concerns, vec!["cost"]);
        assert!(!plan.approved);
        assert!(parse_answer("just prose").findings.is_empty());
    }

    #[test]
    fn claims_need_executed_matching_complete_evidence() {
        let answer = parse_answer(ANSWER);
        let ctx = ctx(Completeness::Complete);
        let mut evidence = RunEvidence::default();
        evidence.read_outputs.insert("c_refunds".into());
        evidence.queries.insert(
            "q1".into(),
            QueryEvidence {
                ok: true,
                text: "region | refunds\nEU | 980\n(1 rows shown)".into(),
                result_hash: Some("h".into()),
                cut_short: false,
            },
        );
        let findings = verify(&answer.findings, &ctx, &evidence, 1);
        assert!(findings[0].verified, "{:?}", findings[0]);
        assert_eq!(findings[0].evidence[0].run.as_deref(), Some("run_7"));
        assert!(
            findings[0].evidence[0]
                .result_hash
                .as_deref()
                .unwrap()
                .starts_with("rows-sha256:")
        );
        assert!(findings[1].verified);
        assert_eq!(findings[0].conflicts_with, vec!["f2"]);
        assert_eq!(findings[1].conflicts_with, vec!["f1"]);
        assert!(findings[2].verified);
        assert_eq!(
            findings[2].evidence[1].group.as_deref(),
            Some("region = EU")
        );
        assert_eq!(
            findings[3].unverified_reason,
            Some(UnverifiedReason::NoEvidence)
        );
        assert_eq!(
            findings[4].unverified_reason,
            Some(UnverifiedReason::UnknownEvidence)
        );

        let mut wrong = answer.findings[0].clone();
        wrong.value = Some("9999".into());
        let checked = verify(&[wrong], &ctx, &evidence, 1);
        assert_eq!(
            checked[0].unverified_reason,
            Some(UnverifiedReason::ValueNotInEvidence)
        );

        let unread = verify(
            &answer.findings[..1],
            &ctx,
            &RunEvidence {
                queries: evidence.queries.clone(),
                read_outputs: BTreeSet::new(),
            },
            1,
        );
        assert_eq!(
            unread[0].unverified_reason,
            Some(UnverifiedReason::UnknownEvidence)
        );

        let sampled = ctx_sampled();
        let checked = verify(&answer.findings[..1], &sampled, &evidence, 1);
        assert_eq!(
            checked[0].unverified_reason,
            Some(UnverifiedReason::IncompleteEvidence)
        );
        let hypothesis = verify(
            &[RawFinding {
                kind: FindingKind::Hypothesis,
                text: "maybe".into(),
                evidence: answer.findings[0].evidence.clone(),
                metric: None,
                value: None,
            }],
            &sampled,
            &evidence,
            1,
        );
        assert!(hypothesis[0].verified, "hypotheses may rest on a sample");

        let nothing = verify(&answer.findings, &ctx, &RunEvidence::default(), 1);
        assert!(
            nothing
                .iter()
                .all(|f| !f.verified && f.unverified_reason == Some(UnverifiedReason::NotExecuted))
        );
    }

    fn ctx_sampled() -> NotebookContext {
        ctx(Completeness::Sampled { n: 1 })
    }

    #[test]
    fn values_match_numerically_or_as_text() {
        assert!(value_in("1,204", "EU | 1204.00"));
        assert!(value_in("$1204", "total 1,204"));
        assert!(value_in("4.1%", "rate 4.1"));
        assert!(!value_in("1205", "EU | 1204"));
        assert!(value_in("Germany", "country | germany"));
        assert!(!value_in("France", "country | germany"));
    }
}
