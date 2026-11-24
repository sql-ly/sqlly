//! What the app and the command line add to a run record, under
//! `extra.details`: one shape for both, so the History tab lists CLI runs
//! and `sqlly-cli notebook runs` reads app runs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cells::assertion::FailingSample;
use super::fingerprint::{ConnectionIdentity, FingerprintParts};
use super::output::{CellOutput, Completeness};
use super::run_record::{RedactedParam, RunRecord, sql_hash};
use crate::plans::ExecutionPlan;

/// Key of the run details inside `RunRecord.extra`.
pub const DETAILS_KEY: &str = "details";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunDetails {
    #[serde(default)]
    pub notebook_title: String,
    /// The notebook's file name (not its full path).
    #[serde(default)]
    pub notebook_file: String,
    /// What was asked for: `all`, `stale`, `cell`, … or the CLI's
    /// `--cells` text.
    #[serde(default)]
    pub request: String,
    /// Every parameter's value as run, redacted where sensitive.
    #[serde(default)]
    pub parameters: Vec<RedactedParam>,
    /// Fingerprint components per cell, so a later run can say why a
    /// fingerprint moved.
    #[serde(default)]
    pub fingerprints: BTreeMap<String, FingerprintParts>,
    /// The connection each slot ran on (opaque profile id, never a secret).
    #[serde(default)]
    pub connections: BTreeMap<String, SlotSummary>,
    #[serde(default)]
    pub outputs: BTreeMap<String, OutputSummary>,
    /// `Vec<AssertionOutcome>` JSON per assertion cell.
    #[serde(default)]
    pub assertions: BTreeMap<String, Value>,
    /// `KpiResult` JSON per KPI cell.
    #[serde(default)]
    pub kpis: BTreeMap<String, Value>,
    /// Failing-row evidence per assertion cell, by assertion position.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assertion_samples: BTreeMap<String, BTreeMap<usize, FailingSample>>,
    /// Captured plans per plan cell.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plans: BTreeMap<String, Vec<ExecutionPlan>>,
    /// Who each cell's query ran as (opaque ids and labels, no secrets):
    /// the scope its stored output is read back under.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub identities: BTreeMap<String, ConnectionIdentity>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotSummary {
    pub label: String,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSummary {
    pub columns: Vec<String>,
    pub row_count: u64,
    pub completeness: Completeness,
    /// `rows-sha256:` over the snapshot's values; equal checksums of
    /// complete outputs mean equal data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    pub elapsed_ms: u64,
}

impl OutputSummary {
    pub fn of(output: &CellOutput) -> Self {
        let encoded = serde_json::to_string(output.rows.as_ref()).unwrap_or_default();
        Self {
            columns: output.columns.iter().map(|c| c.name.clone()).collect(),
            row_count: output.row_count,
            completeness: output.completeness,
            checksum: Some(format!("rows-sha256:{}", sql_hash(&encoded))),
            elapsed_ms: output.elapsed_ms,
        }
    }
}

/// A record's details; empty when it has none or they don't parse.
pub fn details_of(record: &RunRecord) -> RunDetails {
    record
        .extra
        .get(DETAILS_KEY)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn app_and_command_line_details_read_as_one_shape() {
        let cli = json!({"notebook_title": "Orders", "request": "all",
            "outputs": {"c_1": {"columns": ["id"], "row_count": 3, "completeness": {"kind": "complete"},
                                "checksum": "rows-sha256:ab", "elapsed_ms": 4}},
            "kpis": {"c_kpi": {"value": 1.5}}});
        let identity = ConnectionIdentity {
            profile_id: "p-1".into(),
            ..ConnectionIdentity::default()
        };
        let app = json!({"notebook_title": "Orders", "request": "cell",
            "identities": {"c_1": identity},
            "assertion_samples": {"c_t": {"0": {"columns": [], "rows": []}}}});
        for (details, request) in [(cli, "all"), (app, "cell")] {
            let mut record: RunRecord = serde_json::from_value(json!({
                "run_id": "r_1", "notebook_id": "nb", "doc_hash": "h",
                "started_at": "2026-11-18T10:00:00Z", "trigger": "cli",
                "identity_label": "me", "engine_version": "1", "cells": []
            }))
            .unwrap();
            record.extra.insert(DETAILS_KEY.into(), details.clone());
            let read = details_of(&record);
            assert_eq!(read.notebook_title, "Orders");
            assert_eq!(read.request, request);
            let again = serde_json::to_value(&read).unwrap();
            for key in details.as_object().unwrap().keys() {
                assert!(again.get(key).is_some(), "{key} survives a round trip");
            }
        }
    }

    #[test]
    fn output_checksums_follow_the_rows() {
        use sqlly_core::{CellValue, ResultColumn};
        let column = ResultColumn {
            name: "id".into(),
            data_type: "int".into(),
            nullable: false,
        };
        let output = |n: i64| {
            CellOutput::capped(
                vec![column.clone()],
                vec![vec![CellValue::I64(n)]],
                None,
                10,
                1,
                "r_1",
                super::super::fingerprint::Fingerprint("f".into()),
                Default::default(),
            )
        };
        assert_eq!(OutputSummary::of(&output(1)), OutputSummary::of(&output(1)));
        assert_ne!(
            OutputSummary::of(&output(1)).checksum,
            OutputSummary::of(&output(2)).checksum
        );
    }
}
