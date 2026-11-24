//! Run-to-run comparison: for each cell, whether its result moved because
//! its SQL, its spec, its parameters, its connection or policy, an upstream
//! cell, or the data underneath changed, with row-count and test deltas.
//!
//! Fingerprint components (kept with each run) say which input changed.
//! When none did but the output differs, the data changed. When a record
//! lacks the components (older runs), the comparison says so instead of
//! guessing.

use std::collections::BTreeMap;

use serde_json::Value;
use sqlly_services::analytics::assertions::{AssertionOutcome, AssertionStatus};
use sqlly_services::notebook::CellStatus;
use sqlly_services::notebook::fingerprint::{CellFingerprint, ChangeReason};
use sqlly_services::notebook::run_record::{CellRunRecord, RunRecord};

use super::{OutputSummary, RunDetails, details_of};

#[derive(Debug, Clone, PartialEq)]
pub enum ChangeCause {
    /// The cell's SQL text changed.
    Sql,
    /// The cell's configuration changed (non-SQL cells, or its kind).
    Spec,
    /// These parameters it uses changed (names only when values are hidden).
    Parameters(Vec<String>),
    Connection,
    Policy,
    /// Cells it reads from changed.
    Upstream(Vec<String>),
    /// Nothing the notebook controls changed, but the result did.
    Data,
    /// The fingerprint moved and the records do not say why.
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestChange {
    pub name: String,
    pub before: Option<AssertionStatus>,
    pub after: Option<AssertionStatus>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CellComparison {
    pub cell_id: String,
    pub before: Option<CellStatus>,
    pub after: Option<CellStatus>,
    pub causes: Vec<ChangeCause>,
    pub rows_before: Option<u64>,
    pub rows_after: Option<u64>,
    /// Both outputs were complete and checksummed, so "same data" is proven.
    pub data_conclusive: bool,
    pub result_changed: bool,
    pub tests: Vec<TestChange>,
}

impl CellComparison {
    pub fn unchanged(&self) -> bool {
        self.causes.is_empty() && !self.result_changed && self.before == self.after
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParamChange {
    pub name: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    /// The value is sensitive and never recorded; only "it may differ".
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunComparison {
    pub before: String,
    pub after: String,
    pub notebook_changed: bool,
    pub params: Vec<ParamChange>,
    pub cells: Vec<CellComparison>,
}

fn outputs_differ(a: &OutputSummary, b: &OutputSummary) -> (bool, bool) {
    let complete = a.completeness.is_complete() && b.completeness.is_complete();
    if a.row_count != b.row_count || a.columns != b.columns {
        return (true, true);
    }
    match (&a.checksum, &b.checksum) {
        (Some(x), Some(y)) if complete => (x != y, true),
        _ => (false, false),
    }
}

fn outcomes(details: &RunDetails, cell: &str) -> Vec<AssertionOutcome> {
    details
        .assertions
        .get(cell)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

fn test_changes(before: &[AssertionOutcome], after: &[AssertionOutcome]) -> Vec<TestChange> {
    let label =
        |i: usize, o: &AssertionOutcome| o.name.clone().unwrap_or_else(|| format!("#{}", i + 1));
    let n = before.len().max(after.len());
    (0..n)
        .filter_map(|i| {
            let b = before.get(i);
            let a = after.get(i);
            let (bs, as_) = (b.map(|o| o.status), a.map(|o| o.status));
            (bs != as_).then(|| TestChange {
                name: a.or(b).map(|o| label(i, o)).unwrap_or_default(),
                before: bs,
                after: as_,
            })
        })
        .collect()
}

/// Compare run `before` with run `after`. `upstream(cell)` lists the cells
/// a cell reads from (from the current notebook), to name the upstream
/// cells that changed.
pub fn compare_runs(
    before: &RunRecord,
    after: &RunRecord,
    upstream: &dyn Fn(&str) -> Vec<String>,
) -> RunComparison {
    let (bd, ad) = (details_of(before), details_of(after));
    let by_id = |r: &RunRecord| -> BTreeMap<String, CellRunRecord> {
        r.cells
            .iter()
            .map(|c| (c.cell_id.clone(), c.clone()))
            .collect()
    };
    let (bc, ac) = (by_id(before), by_id(after));
    let mut order: Vec<String> = after.cells.iter().map(|c| c.cell_id.clone()).collect();
    for c in &before.cells {
        if !order.contains(&c.cell_id) {
            order.push(c.cell_id.clone());
        }
    }

    let params = param_changes(&bd, &ad);
    let mut cells: Vec<CellComparison> = Vec::new();
    for id in &order {
        let (b, a) = (bc.get(id), ac.get(id));
        let mut cmp = CellComparison {
            cell_id: id.clone(),
            before: b.map(|c| c.status),
            after: a.map(|c| c.status),
            causes: Vec::new(),
            rows_before: b.and_then(|c| c.row_count),
            rows_after: a.and_then(|c| c.row_count),
            data_conclusive: false,
            result_changed: false,
            tests: test_changes(&outcomes(&bd, id), &outcomes(&ad, id)),
        };
        if let (Some(b), Some(a)) = (b, a) {
            let (changed, conclusive) = match (bd.outputs.get(id), ad.outputs.get(id)) {
                (Some(x), Some(y)) => outputs_differ(x, y),
                _ => (b.row_count != a.row_count, false),
            };
            cmp.result_changed = changed || b.row_count != a.row_count;
            cmp.data_conclusive = conclusive;
            if b.fingerprint != a.fingerprint {
                cmp.causes = input_causes(id, b, a, &bd, &ad, upstream, &cells);
            } else if cmp.result_changed {
                cmp.causes.push(ChangeCause::Data);
            }
        }
        cells.push(cmp);
    }
    RunComparison {
        before: before.run_id.clone(),
        after: after.run_id.clone(),
        notebook_changed: before.doc_hash != after.doc_hash,
        params,
        cells,
    }
}

fn input_causes(
    id: &str,
    b: &CellRunRecord,
    a: &CellRunRecord,
    bd: &RunDetails,
    ad: &RunDetails,
    upstream: &dyn Fn(&str) -> Vec<String>,
    earlier: &[CellComparison],
) -> Vec<ChangeCause> {
    let (Some(bp), Some(ap)) = (bd.fingerprints.get(id), ad.fingerprints.get(id)) else {
        // No components: the SQL hash still tells a text change from the rest.
        return if b.sql_hash.is_some() && b.sql_hash != a.sql_hash && b.params == a.params {
            vec![ChangeCause::Sql]
        } else {
            vec![ChangeCause::Unknown]
        };
    };
    let older = CellFingerprint {
        value: b.fingerprint.clone(),
        parts: bp.clone(),
    };
    let newer = CellFingerprint {
        value: a.fingerprint.clone(),
        parts: ap.clone(),
    };
    newer
        .changes_from(&older)
        .into_iter()
        .map(|reason| match reason {
            ChangeReason::Source if a.sql_hash.is_some() || b.sql_hash.is_some() => {
                ChangeCause::Sql
            }
            ChangeReason::Source | ChangeReason::KindOrSpec => ChangeCause::Spec,
            ChangeReason::Connection => ChangeCause::Connection,
            ChangeReason::Policy => ChangeCause::Policy,
            ChangeReason::Parameters => {
                let changed = |hidden: bool| -> Vec<String> {
                    a.params
                        .iter()
                        .filter(|p| p.redacted == hidden)
                        .filter(|p| {
                            b.params
                                .iter()
                                .find(|q| q.name == p.name)
                                .is_none_or(|q| q.value != p.value || hidden)
                        })
                        .map(|p| p.name.clone())
                        .collect()
                };
                // A hidden value can only be the cause when no visible one is.
                let mut names = changed(false);
                if names.is_empty() {
                    names = changed(true);
                }
                if names.is_empty() {
                    names = a.params.iter().map(|p| p.name.clone()).collect();
                }
                ChangeCause::Parameters(names)
            }
            ChangeReason::Upstream => ChangeCause::Upstream(
                upstream(id)
                    .into_iter()
                    .filter(|u| {
                        earlier
                            .iter()
                            .any(|c| &c.cell_id == u && (!c.causes.is_empty() || c.result_changed))
                    })
                    .collect(),
            ),
        })
        .collect()
}

fn param_changes(before: &RunDetails, after: &RunDetails) -> Vec<ParamChange> {
    let mut names: Vec<&str> = after.parameters.iter().map(|p| p.name.as_str()).collect();
    for p in &before.parameters {
        if !names.contains(&p.name.as_str()) {
            names.push(&p.name);
        }
    }
    names
        .into_iter()
        .filter_map(|name| {
            let b = before.parameters.iter().find(|p| p.name == name);
            let a = after.parameters.iter().find(|p| p.name == name);
            let hidden = b.is_some_and(|p| p.redacted) || a.is_some_and(|p| p.redacted);
            let (bv, av) = (
                b.and_then(|p| p.value.clone()),
                a.and_then(|p| p.value.clone()),
            );
            (bv != av || b.is_none() != a.is_none()).then(|| ParamChange {
                name: name.to_string(),
                before: bv,
                after: av,
                hidden,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use serde_json::json;
    use sqlly_services::notebook::Completeness;
    use sqlly_services::notebook::document::Extra;
    use sqlly_services::notebook::fingerprint::{Fingerprint, FingerprintParts};
    use sqlly_services::notebook::run_record::{RedactedParam, RunTrigger};

    fn fp(text: &str) -> Fingerprint {
        Fingerprint(text.into())
    }

    fn parts(source: &str, params: &str, upstream: &str) -> FingerprintParts {
        FingerprintParts {
            source: fp(source),
            kind_spec: fp("k"),
            connection: fp("c"),
            policy: fp("p"),
            params: fp(params),
            upstream: fp(upstream),
        }
    }

    struct Cell {
        id: &'static str,
        parts: FingerprintParts,
        rows: u64,
        checksum: &'static str,
        region: Option<&'static str>,
    }

    fn run(run_id: &str, cells: &[Cell]) -> RunRecord {
        let mut details = RunDetails::default();
        let mut records = Vec::new();
        for c in cells {
            let p = &c.parts;
            let value = fp(&format!("{}{}{}", p.source.0, p.params.0, p.upstream.0));
            details.fingerprints.insert(c.id.into(), p.clone());
            details.outputs.insert(
                c.id.into(),
                OutputSummary {
                    columns: vec!["n".into()],
                    row_count: c.rows,
                    completeness: Completeness::Complete,
                    checksum: Some(c.checksum.into()),
                    elapsed_ms: 1,
                },
            );
            let params: Vec<RedactedParam> = c
                .region
                .map(|r| RedactedParam {
                    name: "region".into(),
                    type_tag: "text".into(),
                    value: Some(json!(r)),
                    redacted: false,
                })
                .into_iter()
                .collect();
            records.push(CellRunRecord {
                cell_id: c.id.into(),
                fingerprint: value,
                status: CellStatus::Fresh,
                started_at: None,
                ended_at: None,
                row_count: Some(c.rows),
                completeness: Some(Completeness::Complete),
                reused: false,
                sql_hash: Some(format!("{}{}", p.source.0, p.params.0)),
                error: None,
                params,
            });
        }
        let mut extra = Extra::new();
        extra.insert(
            super::super::DETAILS_KEY.into(),
            serde_json::to_value(&details).unwrap(),
        );
        RunRecord {
            run_id: run_id.into(),
            notebook_id: "nb".into(),
            doc_hash: "d".into(),
            started_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            ended_at: None,
            trigger: RunTrigger::Manual,
            identity_label: String::new(),
            engine_version: String::new(),
            cells: records,
            extra,
        }
    }

    fn orders(source: &str, region: &'static str, rows: u64, sum: &'static str) -> Cell {
        Cell {
            id: "orders",
            parts: parts(source, region, "-"),
            rows,
            checksum: sum,
            region: Some(region),
        }
    }

    fn chart(upstream: &str, rows: u64, sum: &'static str) -> Cell {
        Cell {
            id: "chart",
            parts: parts("chart", "-", upstream),
            rows,
            checksum: sum,
            region: None,
        }
    }

    fn upstream(cell: &str) -> Vec<String> {
        if cell == "chart" {
            vec!["orders".into()]
        } else {
            Vec::new()
        }
    }

    #[test]
    fn an_sql_change_is_explained_as_sql() {
        let a = run("r1", &[orders("s1", "EMEA", 10, "x"), chart("u1", 3, "y")]);
        let b = run("r2", &[orders("s2", "EMEA", 12, "z"), chart("u2", 4, "w")]);
        let cmp = compare_runs(&a, &b, &upstream);
        assert_eq!(cmp.cells[0].causes, vec![ChangeCause::Sql]);
        assert_eq!(
            (cmp.cells[0].rows_before, cmp.cells[0].rows_after),
            (Some(10), Some(12))
        );
        assert_eq!(
            cmp.cells[1].causes,
            vec![ChangeCause::Upstream(vec!["orders".into()])]
        );
        assert!(cmp.params.is_empty());
    }

    #[test]
    fn a_parameter_change_names_the_parameter() {
        let a = run("r1", &[orders("s1", "EMEA", 10, "x")]);
        let b = run("r2", &[orders("s1", "APAC", 7, "q")]);
        let cmp = compare_runs(&a, &b, &upstream);
        assert_eq!(
            cmp.cells[0].causes,
            vec![ChangeCause::Parameters(vec!["region".into()])]
        );
    }

    #[test]
    fn same_inputs_with_different_results_is_a_data_change() {
        let a = run("r1", &[orders("s1", "EMEA", 10, "x"), chart("u1", 3, "y")]);
        let same_count = run(
            "r2",
            &[orders("s1", "EMEA", 10, "CHANGED"), chart("u1", 3, "y")],
        );
        let cmp = compare_runs(&a, &same_count, &upstream);
        assert_eq!(cmp.cells[0].causes, vec![ChangeCause::Data]);
        assert!(cmp.cells[0].data_conclusive && cmp.cells[0].result_changed);
        assert!(cmp.cells[1].unchanged());
        let identical = compare_runs(&a, &a.clone(), &upstream);
        assert!(identical.cells.iter().all(CellComparison::unchanged));
    }

    #[test]
    fn records_without_components_say_they_cannot_tell() {
        let a = run("r1", &[orders("s1", "EMEA", 10, "x")]);
        let mut b = run("r2", &[orders("s1", "APAC", 10, "x")]);
        b.extra.clear();
        let cmp = compare_runs(&a, &b, &upstream);
        assert_eq!(cmp.cells[0].causes, vec![ChangeCause::Unknown]);
    }

    #[test]
    fn test_outcomes_and_hidden_parameters_are_compared() {
        let mut a = run("r1", &[orders("s1", "EMEA", 10, "x")]);
        let mut b = run("r2", &[orders("s1", "EMEA", 10, "x")]);
        let outcome = |status: &str| {
            json!([{"name": "unique id", "status": status, "issue": null,
                    "evidence": {"observed": {}, "checked_rows": 10, "failing_count": 0,
                                 "failing_rows": [], "failing_values": [], "partial": false}}])
        };
        for (r, status, token) in [(&mut a, "pass", None), (&mut b, "fail", Some("x"))] {
            let mut d = details_of(r);
            d.assertions.insert("tests".into(), outcome(status));
            d.parameters.push(RedactedParam {
                name: "token".into(),
                type_tag: "text".into(),
                value: token.map(|t: &str| json!(t)),
                redacted: token.is_none(),
            });
            r.extra.insert(
                super::super::DETAILS_KEY.into(),
                serde_json::to_value(&d).unwrap(),
            );
        }
        b.cells.push(a.cells[0].clone());
        b.cells[1].cell_id = "tests".into();
        let cmp = compare_runs(&a, &b, &upstream);
        let tests = cmp.cells.iter().find(|c| c.cell_id == "tests").unwrap();
        assert_eq!(
            tests.tests,
            vec![TestChange {
                name: "unique id".into(),
                before: Some(AssertionStatus::Pass),
                after: Some(AssertionStatus::Fail),
            }]
        );
        assert!(tests.before.is_none());
        assert_eq!(cmp.params.len(), 1);
        assert!(cmp.params[0].hidden);
    }
}
