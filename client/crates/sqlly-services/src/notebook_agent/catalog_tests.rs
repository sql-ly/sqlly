//! Every catalog message the agent stores in a notebook (refusals, Review
//! and Explain changes findings) exists in `en.json`, and its English
//! template filled with the stored arguments is the stored English text, so
//! the app can re-render it in any language.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use sqlly_core::{CellValue, ResultColumn, StoredMessage};

use super::context::{CellRuntime, NotebookContext, observed};
use super::explain::{explain_cell, explain_since};
use super::proposals::Refusal;
use super::review::{ambiguous_term_findings, review};
use crate::notebook::cells::ai::Finding;
use crate::notebook::fingerprint::{Fingerprint, FingerprintParts};
use crate::notebook::params::{ParamType, ParameterDef};
use crate::notebook::{
    Cell, CellKind, CellOutput, Completeness, ExecutionPolicy, Freshness, NotebookDoc, Provenance,
    SeededIds,
};
use crate::notebook_security::TrustDecision;

const EN: &str = include_str!("../../../sqlly-i18n/translations/en.json");

fn template(catalog: &Value, key: &str) -> String {
    let mut node = catalog;
    for part in key.split('.') {
        node = node
            .get(part)
            .unwrap_or_else(|| panic!("{key} missing from en.json"));
    }
    node["value"].as_str().unwrap().to_string()
}

fn filled(catalog: &Value, message: &StoredMessage) -> String {
    let mut text = template(catalog, &message.key);
    for (name, value) in &message.arguments {
        let placeholder = format!("{{{name}}}");
        assert!(
            text.contains(&placeholder),
            "{} lacks {placeholder}",
            message.key
        );
        text = text.replace(&placeholder, value);
    }
    assert!(
        !text.contains('{'),
        "{} has unfilled placeholders: {text}",
        message.key
    );
    text
}

fn output(
    columns: &[&str],
    rows: Vec<Vec<CellValue>>,
    completeness: Completeness,
    freshness: Freshness,
) -> CellOutput {
    CellOutput {
        columns: columns
            .iter()
            .map(|c| ResultColumn {
                name: c.to_string(),
                data_type: "int".into(),
                nullable: true,
            })
            .collect(),
        row_count: 500,
        rows: Arc::new(rows),
        completeness,
        elapsed_ms: 1,
        run_id: "r1".into(),
        fingerprint: Fingerprint("f".into()),
        provenance: Provenance::default(),
        freshness,
    }
}

fn sql(id: &str, source: &str, read_only: bool) -> Cell {
    let mut cell = Cell::new(id, CellKind::Sql, ExecutionPolicy::default());
    cell.source = source.into();
    cell.execution_policy.read_only = read_only;
    cell
}

fn review_findings() -> Vec<Finding> {
    let mut doc = NotebookDoc::new("t", &mut SeededIds(3));
    doc.cells = vec![
        sql(
            "c_fan",
            "SELECT o.Region, SUM(o.Freight), COUNT(*) FROM Orders o JOIN OrderLines d ON d.OrderID = o.OrderID WHERE o.At > NOW() GROUP BY o.Region",
            true,
        ),
        sql(
            "c_lines",
            "SELECT o.OrderID FROM Orders o JOIN OrderLines d ON d.OrderID = o.OrderID",
            true,
        ),
        sql(
            "c_star",
            "SELECT COUNT(*), SUM(DISTINCT a.x) FROM a JOIN b ON a.k = b.k",
            true,
        ),
        sql("c_filtered", "SELECT * FROM t WHERE r = :region", true),
        sql("c_write", "UPDATE t SET x = 1", false),
    ];
    for i in 0..4 {
        doc.cells
            .push(sql(&format!("c_cache{i}"), "SELECT 1", true));
    }
    for i in 0..3 {
        doc.cells.push(sql(&format!("c_part{i}"), "SELECT 1", true));
    }
    doc.parameters
        .push(ParameterDef::new("region", ParamType::String));
    let mut outputs = BTreeMap::new();
    outputs.insert(
        "c_lines".to_string(),
        output(
            &["OrderID"],
            (0..6).map(|i| vec![CellValue::I64(i / 2)]).collect(),
            Completeness::Complete,
            Freshness::Live,
        ),
    );
    for (i, freshness) in [
        Freshness::TtlCached,
        Freshness::Pinned,
        Freshness::SourceVerified,
        Freshness::Snapshot,
    ]
    .into_iter()
    .enumerate()
    {
        outputs.insert(
            format!("c_cache{i}"),
            output(
                &["n", "password"],
                vec![],
                Completeness::Complete,
                freshness,
            ),
        );
    }
    for (i, completeness) in [
        Completeness::Sampled { n: 1 },
        Completeness::Partial,
        Completeness::Truncated { cap: 1 },
    ]
    .into_iter()
    .enumerate()
    {
        outputs.insert(
            format!("c_part{i}"),
            output(
                &["n"],
                vec![vec![CellValue::I64(1)]],
                completeness,
                Freshness::Live,
            ),
        );
    }
    let mut runtime = BTreeMap::new();
    runtime.insert(
        "c_fan".to_string(),
        CellRuntime {
            stale: true,
            ..CellRuntime::default()
        },
    );
    let ctx = NotebookContext {
        trust: TrustDecision::untrusted(&doc),
        doc,
        outputs,
        runtime,
        connections: BTreeMap::new(),
        ai_cell: "c_ai".into(),
    };
    let mut findings = review(&ctx, &[]);
    findings.extend(ambiguous_term_findings(&["churn".into()], 1));
    findings
}

fn parts(seed: &str) -> FingerprintParts {
    let f = |s: &str| Fingerprint(format!("{s}{seed}"));
    FingerprintParts {
        source: f("s"),
        kind_spec: f("k"),
        connection: f("c"),
        policy: f("p"),
        params: f("q"),
        upstream: f("u"),
    }
}

fn explain_findings() -> Vec<Finding> {
    let mut out = vec![];
    let a = output(
        &["n"],
        vec![vec![CellValue::I64(1)]],
        Completeness::Complete,
        Freshness::Live,
    );
    let mut b = output(
        &["n", "m"],
        vec![vec![CellValue::I64(1), CellValue::I64(2)]],
        Completeness::Complete,
        Freshness::Live,
    );
    b.run_id = "r2".into();
    let mut id = 0;
    out.extend(explain_cell(
        "c",
        "orders",
        &observed(&a, Some(parts("1"))),
        &observed(&b, Some(parts("2"))),
        Some((&a, &b)),
        &mut id,
    ));
    out.extend(explain_cell(
        "c",
        "orders",
        &observed(&a, Some(parts("1"))),
        &observed(&b, Some(parts("1"))),
        None,
        &mut id,
    ));
    out.extend(explain_cell(
        "c",
        "orders",
        &observed(&a, Some(parts("1"))),
        &observed(&a, Some(parts("1"))),
        None,
        &mut id,
    ));
    let mut changed_param = parts("1");
    changed_param.params = Fingerprint("other".into());
    out.extend(explain_cell(
        "c",
        "orders",
        &observed(&a, Some(parts("1"))),
        &observed(&b, Some(changed_param)),
        None,
        &mut id,
    ));
    let doc = NotebookDoc::new("t", &mut SeededIds(4));
    let ctx = NotebookContext {
        trust: TrustDecision::untrusted(&doc),
        doc,
        outputs: BTreeMap::new(),
        runtime: BTreeMap::new(),
        connections: BTreeMap::new(),
        ai_cell: "c_ai".into(),
    };
    out.extend(explain_since(&ctx, &BTreeMap::new()));
    out.extend(explain_since(
        &ctx,
        &BTreeMap::from([("gone".to_string(), observed(&a, None))]),
    ));
    out.extend(super::explain::explain_runs(
        &ctx,
        &super::explain::RecordedRun::default(),
        None,
    ));
    out
}

#[test]
fn stored_messages_match_the_english_catalog() {
    let catalog: Value = serde_json::from_str::<Value>(EN).unwrap()["notebook"]["ai"].clone();
    let root = serde_json::json!({"notebook": {"ai": catalog}});
    let mut seen = std::collections::BTreeSet::new();
    for finding in review_findings().iter().chain(explain_findings().iter()) {
        let message = finding
            .message
            .as_ref()
            .expect("deterministic findings carry a message");
        assert_eq!(filled(&root, message), finding.text, "{}", message.key);
        seen.insert(message.key.clone());
    }
    let x = || "x".to_string();
    for refusal in [
        Refusal::WriteStatement { cell: Some(x()) },
        Refusal::WriteStatement { cell: None },
        Refusal::WriteCell { cell: x() },
        Refusal::UnknownCell { cell: x() },
        Refusal::UnknownConnection { connection: x() },
        Refusal::UnsupportedKind { kind: x() },
        Refusal::SelfEdit,
        Refusal::NotRunnable { cell: x() },
        Refusal::InvalidSpec,
        Refusal::InvalidKindSpec { kind: x() },
        Refusal::ChangesWhereItRuns { cell: x() },
    ] {
        assert_eq!(
            filled(&root, &refusal.stored()),
            refusal.english(),
            "{}",
            refusal.refusal_key()
        );
        seen.insert(refusal.refusal_key().to_string());
    }
    for group in ["review", "explain", "refusal"] {
        let in_catalog: Vec<String> = root["notebook"]["ai"][group]
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| !k.ends_with("_prompt") && *k != "button")
            .map(|k| format!("notebook.ai.{group}.{k}"))
            .collect();
        for key in in_catalog {
            assert!(
                seen.contains(&key),
                "{key} is in the catalog but no test produced it"
            );
        }
    }
}
