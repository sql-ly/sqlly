//! `sqlly notebook runs`: run history, one run, and why two runs differ.
//!
//! A comparison explains each cell: its fingerprint moved because the SQL,
//! the kind or spec, the connection, the policy, the parameters or an
//! upstream cell changed (from the fingerprint components each run
//! records); or the fingerprint is the same but the data is not (the
//! outputs' checksums or row counts differ, so the source data changed).
//! Data is only called unchanged when both outputs were complete.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;
use sqlly_services::notebook::fingerprint::{ChangeReason, FingerprintParts};
use sqlly_services::notebook::run_record::CellRunRecord;
use sqlly_services::notebook::{CellFingerprint, CellStatus, RunRecord};

use super::NotebookFormat;
use super::records::{NotebookKey, RunDetails, RunsDir, details_of};
use crate::agent::{CliError, ExitCode};

#[derive(Debug, Clone, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct RunsArgs {
    #[command(subcommand)]
    pub action: Option<RunsAction>,
    /// Notebook whose runs to list.
    pub file: Option<PathBuf>,
    /// Show only the newest N runs.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[arg(long, value_enum, default_value_t = NotebookFormat::Table)]
    pub format: NotebookFormat,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum RunsAction {
    /// Show one recorded run.
    Show {
        /// Run id (or a unique prefix); `latest` with --notebook.
        run: String,
        /// Look the run up in this notebook's history.
        #[arg(long)]
        notebook: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = NotebookFormat::Table)]
        format: NotebookFormat,
    },
    /// Explain what changed between two runs, cell by cell.
    Compare {
        /// The older run (id, prefix, or `latest~1` with --notebook).
        a: String,
        /// The newer run (id, prefix, or `latest` with --notebook).
        b: String,
        #[arg(long)]
        notebook: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = NotebookFormat::Table)]
        format: NotebookFormat,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataVerdict {
    /// Same fingerprint, same complete output.
    Unchanged,
    /// Same fingerprint, different output: the source data changed.
    Changed,
    /// One side is incomplete or has no checksum, so it cannot be told.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CellComparison {
    pub cell_id: String,
    /// Only in one of the runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only_in: Option<&'static str>,
    pub fingerprint_changed: bool,
    /// Why the fingerprint moved (when both runs recorded components).
    pub reasons: Vec<ChangeReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<DataVerdict>,
    pub status: (Option<CellStatus>, Option<CellStatus>),
    pub rows: (Option<u64>, Option<u64>),
    /// Assertion outcome changes: `check: pass -> fail`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kpi: Option<(Option<f64>, Option<f64>)>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunComparison {
    pub a: String,
    pub b: String,
    pub same_notebook: bool,
    pub notebook_changed: bool,
    /// Parameters whose recorded value differs (`name`, before, after);
    /// redacted values compare as unknown.
    pub parameters: Vec<(String, Option<Value>, Option<Value>)>,
    /// Slots that ran on a different connection.
    pub connections: Vec<String>,
    pub cells: Vec<CellComparison>,
}

fn fingerprint(record: &CellRunRecord, details: &RunDetails) -> Option<CellFingerprint> {
    let parts: &FingerprintParts = details.fingerprints.get(&record.cell_id)?;
    Some(CellFingerprint {
        value: record.fingerprint.clone(),
        parts: parts.clone(),
    })
}

fn data_verdict(a: &RunDetails, b: &RunDetails, cell: &str) -> DataVerdict {
    match (a.outputs.get(cell), b.outputs.get(cell)) {
        (Some(x), Some(y)) => {
            let complete =
                x.completeness.supports_summaries() && y.completeness.supports_summaries();
            if x.row_count != y.row_count || (complete && x.checksum != y.checksum) {
                DataVerdict::Changed
            } else if complete && x.checksum.is_some() {
                DataVerdict::Unchanged
            } else {
                DataVerdict::Unknown
            }
        }
        _ => DataVerdict::Unknown,
    }
}

fn assertion_statuses(details: &RunDetails, cell: &str) -> Vec<(String, String)> {
    details
        .assertions
        .get(cell)
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .enumerate()
                .map(|(i, o)| {
                    let name = o
                        .get("name")
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("check {}", i + 1), str::to_string);
                    let status = o
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    (name, status)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn kpi_value(details: &RunDetails, cell: &str) -> Option<f64> {
    details.kpis.get(cell)?.get("value")?.as_f64()
}

/// Compare run `a` (older) with run `b` (newer).
pub fn compare(a: &RunRecord, b: &RunRecord) -> RunComparison {
    let (da, db) = (details_of(a), details_of(b));
    let before: BTreeMap<&str, &CellRunRecord> =
        a.cells.iter().map(|c| (c.cell_id.as_str(), c)).collect();
    let after: BTreeMap<&str, &CellRunRecord> =
        b.cells.iter().map(|c| (c.cell_id.as_str(), c)).collect();
    let mut order: Vec<&str> = b.cells.iter().map(|c| c.cell_id.as_str()).collect();
    order.extend(
        a.cells
            .iter()
            .map(|c| c.cell_id.as_str())
            .filter(|id| !after.contains_key(id)),
    );

    let mut cells = Vec::new();
    for id in order {
        let (x, y) = (before.get(id), after.get(id));
        let only_in = match (x, y) {
            (Some(_), None) => Some("a"),
            (None, Some(_)) => Some("b"),
            _ => None,
        };
        let mut row = CellComparison {
            cell_id: id.to_string(),
            only_in,
            fingerprint_changed: false,
            reasons: Vec::new(),
            data: None,
            status: (x.map(|r| r.status), y.map(|r| r.status)),
            rows: (x.and_then(|r| r.row_count), y.and_then(|r| r.row_count)),
            assertions: Vec::new(),
            kpi: None,
        };
        if let (Some(x), Some(y)) = (x, y) {
            row.fingerprint_changed = x.fingerprint != y.fingerprint;
            if row.fingerprint_changed {
                if let (Some(old), Some(new)) = (fingerprint(x, &da), fingerprint(y, &db)) {
                    row.reasons = new.changes_from(&old);
                }
            } else {
                row.data = Some(data_verdict(&da, &db, id));
            }
            let old_checks: BTreeMap<String, String> =
                assertion_statuses(&da, id).into_iter().collect();
            for (name, status) in assertion_statuses(&db, id) {
                match old_checks.get(&name) {
                    Some(prev) if *prev != status => {
                        row.assertions.push(format!("{name}: {prev} -> {status}"));
                    }
                    None => row.assertions.push(format!("{name}: new, {status}")),
                    _ => {}
                }
            }
            let (ka, kb) = (kpi_value(&da, id), kpi_value(&db, id));
            if (ka.is_some() || kb.is_some()) && ka != kb {
                row.kpi = Some((ka, kb));
            }
        }
        let interesting = row.only_in.is_some()
            || row.fingerprint_changed
            || row.data == Some(DataVerdict::Changed)
            || row.status.0 != row.status.1
            || row.rows.0 != row.rows.1
            || !row.assertions.is_empty()
            || row.kpi.is_some();
        if interesting {
            cells.push(row);
        }
    }

    let params_a: BTreeMap<String, (Option<Value>, bool)> = da
        .parameters
        .iter()
        .map(|p| (p.name.clone(), (p.value.clone(), p.redacted)))
        .collect();
    let mut parameters = Vec::new();
    let names: BTreeSet<String> = params_a
        .keys()
        .cloned()
        .chain(db.parameters.iter().map(|p| p.name.clone()))
        .collect();
    for name in names {
        let old = params_a.get(&name);
        let new = db.parameters.iter().find(|p| p.name == name);
        let redacted = old.is_some_and(|o| o.1) || new.is_some_and(|n| n.redacted);
        let (ov, nv) = (
            old.and_then(|o| o.0.clone()),
            new.and_then(|n| n.value.clone()),
        );
        if !redacted && ov != nv {
            parameters.push((name, ov, nv));
        }
    }
    let slots: BTreeSet<&String> = da.connections.keys().chain(db.connections.keys()).collect();
    let connections = slots
        .into_iter()
        .filter(|slot| {
            da.connections
                .get(*slot)
                .map(|s| (&s.profile_id, &s.database))
                != db
                    .connections
                    .get(*slot)
                    .map(|s| (&s.profile_id, &s.database))
        })
        .cloned()
        .collect();

    RunComparison {
        a: a.run_id.clone(),
        b: b.run_id.clone(),
        same_notebook: a.notebook_id == b.notebook_id,
        notebook_changed: a.doc_hash != b.doc_hash,
        parameters,
        connections,
        cells,
    }
}

fn reason_text(reason: ChangeReason) -> &'static str {
    match reason {
        ChangeReason::Source => "SQL changed",
        ChangeReason::KindOrSpec => "cell settings changed",
        ChangeReason::Connection => "connection changed",
        ChangeReason::Policy => "run policy changed",
        ChangeReason::Parameters => "parameters changed",
        ChangeReason::Upstream => "an input cell changed",
    }
}

fn status_word(status: Option<CellStatus>) -> String {
    status
        .and_then(|s| serde_json::to_value(s).ok())
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "-".into())
}

fn opt<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "-".into(), |v| v.to_string())
}

pub fn render_comparison(cmp: &RunComparison, labels: &BTreeMap<String, String>) -> String {
    let mut out = format!("{} -> {}\n", cmp.a, cmp.b);
    if !cmp.same_notebook {
        out.push_str("note: the runs belong to different notebooks\n");
    }
    if cmp.notebook_changed {
        out.push_str("the notebook file changed between the runs\n");
    }
    for (name, before, after) in &cmp.parameters {
        let show = |v: &Option<Value>| v.as_ref().map_or("(none)".into(), Value::to_string);
        out.push_str(&format!(
            "parameter {name}: {} -> {}\n",
            show(before),
            show(after)
        ));
    }
    for slot in &cmp.connections {
        let slot = if slot.is_empty() {
            "(default)"
        } else {
            slot.as_str()
        };
        out.push_str(&format!(
            "connection {slot}: ran on a different connection\n"
        ));
    }
    if cmp.cells.is_empty() {
        out.push_str("no cell changed");
        return out;
    }
    for cell in &cmp.cells {
        let label = labels
            .get(&cell.cell_id)
            .cloned()
            .unwrap_or_else(|| cell.cell_id.clone());
        let mut parts: Vec<String> = Vec::new();
        if let Some(side) = cell.only_in {
            parts.push(format!("only in run {side}"));
        }
        if cell.fingerprint_changed {
            if cell.reasons.is_empty() {
                parts.push("fingerprint changed".into());
            } else {
                parts.extend(cell.reasons.iter().map(|r| reason_text(*r).to_string()));
            }
        }
        match cell.data {
            Some(DataVerdict::Changed) => parts.push("same query, data changed".into()),
            Some(DataVerdict::Unknown) if cell.rows.0 == cell.rows.1 => {}
            _ => {}
        }
        if cell.status.0 != cell.status.1 && cell.only_in.is_none() {
            parts.push(format!(
                "status {} -> {}",
                status_word(cell.status.0),
                status_word(cell.status.1)
            ));
        }
        if cell.rows.0 != cell.rows.1 && cell.only_in.is_none() {
            parts.push(format!("rows {} -> {}", opt(cell.rows.0), opt(cell.rows.1)));
        }
        if let Some((x, y)) = cell.kpi {
            parts.push(format!("value {} -> {}", opt(x), opt(y)));
        }
        parts.extend(cell.assertions.iter().cloned());
        out.push_str(&format!("{label}: {}\n", parts.join("; ")));
    }
    out.trim_end().to_string()
}

fn run_line(record: &RunRecord) -> Value {
    let details = details_of(record);
    let count = |status: CellStatus| record.cells.iter().filter(|c| c.status == status).count();
    let failed_checks: usize = details
        .assertions
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("fail"))
        .count();
    serde_json::json!({
        "run_id": record.run_id,
        "started_at": record.started_at,
        "ended_at": record.ended_at,
        "trigger": record.trigger,
        "request": details.request,
        "fresh": count(CellStatus::Fresh),
        "reused": record.cells.iter().filter(|c| c.reused).count(),
        "failed": count(CellStatus::Failed),
        "blocked": count(CellStatus::Blocked),
        "failed_checks": failed_checks,
    })
}

fn render_list(lines: &[Value]) -> String {
    use comfy_table::presets::UTF8_FULL_CONDENSED;
    use comfy_table::{ContentArrangement, Table};
    if lines.is_empty() {
        return "no recorded runs".into();
    }
    let mut table = Table::new();
    table.load_style(UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Disabled);
    table.set_header([
        "run",
        "started",
        "trigger",
        "ran",
        "reused",
        "failed",
        "blocked",
        "failed checks",
    ]);
    for line in lines {
        let get = |key: &str| match &line[key] {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        table.add_row([
            get("run_id"),
            get("started_at"),
            get("trigger"),
            get("fresh"),
            get("reused"),
            get("failed"),
            get("blocked"),
            get("failed_checks"),
        ]);
    }
    table.to_string()
}

fn render_show(record: &RunRecord) -> String {
    let details = details_of(record);
    let mut out = format!(
        "run {} of \"{}\" ({:?}, {})\nstarted {}",
        record.run_id,
        details.notebook_title,
        record.trigger,
        record.identity_label,
        record.started_at
    );
    if let Some(end) = record.ended_at {
        out.push_str(&format!(", ended {end}"));
    }
    out.push('\n');
    for p in &details.parameters {
        let value = if p.redacted {
            "[redacted]".to_string()
        } else {
            p.value.as_ref().map_or("(none)".into(), Value::to_string)
        };
        out.push_str(&format!("parameter {}: {value}\n", p.name));
    }
    for cell in &record.cells {
        let mut line = format!("{}: {}", cell.cell_id, status_word(Some(cell.status)));
        if let Some(rows) = cell.row_count {
            line.push_str(&format!(", {rows} rows"));
        }
        if cell.reused {
            line.push_str(", reused");
        }
        if let Some(error) = &cell.error {
            line.push_str(&format!(" - {}", error.message));
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn find(runs: &RunsDir, notebook: Option<&PathBuf>, spec: &str) -> Result<RunRecord, CliError> {
    match notebook {
        Some(path) => runs.resolve_run(&NotebookKey::of_file(path, runs)?, spec),
        None if spec.starts_with("latest") => Err(CliError::usage(
            "`latest` needs --notebook <file> to know which notebook",
        )),
        None => runs.find_run(spec),
    }
}

fn labels_for(notebook: Option<&PathBuf>) -> BTreeMap<String, String> {
    notebook
        .and_then(|p| super::source::load_file(p).ok())
        .map(|l| {
            l.doc
                .cells
                .iter()
                .map(|c| (c.id.clone(), c.relation_name().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn command(args: &RunsArgs, runs: &RunsDir, json: bool) -> ExitCode {
    match &args.action {
        None => {
            let Some(file) = &args.file else {
                return crate::agent::emit(
                    json,
                    None,
                    Err(CliError::usage(
                        "give a notebook file, or `runs show` / `runs compare`",
                    )),
                );
            };
            let result = NotebookKey::of_file(file, runs).and_then(|key| runs.list(&key));
            match result {
                Ok(records) => {
                    let lines: Vec<Value> = records
                        .iter()
                        .rev()
                        .take(args.limit.max(1))
                        .map(run_line)
                        .collect();
                    let text = render_list(&lines);
                    super::finish(
                        json,
                        args.format,
                        text,
                        serde_json::json!({"runs": lines}),
                        ExitCode::Ok,
                    )
                }
                Err(err) => crate::agent::emit(json, None, Err(err)),
            }
        }
        Some(RunsAction::Show {
            run,
            notebook,
            format,
        }) => match find(runs, notebook.as_ref(), run) {
            Ok(record) => super::finish(
                json,
                *format,
                render_show(&record),
                serde_json::to_value(&record).unwrap_or(Value::Null),
                ExitCode::Ok,
            ),
            Err(err) => crate::agent::emit(json, None, Err(err)),
        },
        Some(RunsAction::Compare {
            a,
            b,
            notebook,
            format,
        }) => {
            let pair = find(runs, notebook.as_ref(), a)
                .and_then(|x| Ok((x, find(runs, notebook.as_ref(), b)?)));
            match pair {
                Ok((x, y)) => {
                    let cmp = compare(&x, &y);
                    let text = render_comparison(&cmp, &labels_for(notebook.as_ref()));
                    super::finish(
                        json,
                        *format,
                        text,
                        serde_json::to_value(&cmp).unwrap_or(Value::Null),
                        ExitCode::Ok,
                    )
                }
                Err(err) => crate::agent::emit(json, None, Err(err)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::notebook::records::{DETAILS_KEY, OutputSummary};
    use chrono::{TimeZone, Utc};
    use sqlly_services::notebook::Completeness;
    use sqlly_services::notebook::fingerprint::Fingerprint;
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

    fn cell(id: &str, value: &str, rows: u64) -> CellRunRecord {
        CellRunRecord {
            cell_id: id.into(),
            fingerprint: fp(value),
            status: CellStatus::Fresh,
            started_at: None,
            ended_at: None,
            row_count: Some(rows),
            completeness: Some(Completeness::Complete),
            reused: false,
            sql_hash: None,
            error: None,
            params: Vec::new(),
        }
    }

    fn run(id: &str, cells: Vec<CellRunRecord>, details: RunDetails) -> RunRecord {
        RunRecord {
            run_id: id.into(),
            notebook_id: "nb".into(),
            doc_hash: "h".into(),
            started_at: Utc.with_ymd_and_hms(2026, 11, 18, 7, 0, 0).unwrap(),
            ended_at: None,
            trigger: RunTrigger::Cli,
            identity_label: "me".into(),
            engine_version: "test".into(),
            cells,
            extra: BTreeMap::from([(
                DETAILS_KEY.to_string(),
                serde_json::to_value(details).unwrap(),
            )]),
        }
    }

    fn output(rows: u64, checksum: &str, completeness: Completeness) -> OutputSummary {
        OutputSummary {
            columns: vec!["n".into()],
            row_count: rows,
            completeness,
            checksum: Some(checksum.into()),
            elapsed_ms: 1,
        }
    }

    #[test]
    fn explains_sql_params_upstream_and_data_changes() {
        let mut da = RunDetails::default();
        let mut db = RunDetails::default();
        // orders: params changed; summary: only upstream changed; stable:
        // same fingerprint but new data; same: nothing changed.
        da.fingerprints
            .insert("orders".into(), parts("s", "p1", "u"));
        db.fingerprints
            .insert("orders".into(), parts("s", "p2", "u"));
        da.fingerprints
            .insert("summary".into(), parts("s", "p", "u1"));
        db.fingerprints
            .insert("summary".into(), parts("s", "p", "u2"));
        da.fingerprints
            .insert("edited".into(), parts("s1", "p", "u"));
        db.fingerprints
            .insert("edited".into(), parts("s2", "p", "u"));
        da.outputs
            .insert("stable".into(), output(5, "x", Completeness::Complete));
        db.outputs
            .insert("stable".into(), output(5, "y", Completeness::Complete));
        da.outputs
            .insert("same".into(), output(5, "x", Completeness::Complete));
        db.outputs
            .insert("same".into(), output(5, "x", Completeness::Complete));
        da.parameters.push(RedactedParam {
            name: "region".into(),
            type_tag: "text".into(),
            value: Some(Value::String("EU".into())),
            redacted: false,
        });
        db.parameters.push(RedactedParam {
            name: "region".into(),
            type_tag: "text".into(),
            value: Some(Value::String("US".into())),
            redacted: false,
        });
        let a = run(
            "r_a",
            vec![
                cell("orders", "1", 10),
                cell("summary", "2", 1),
                cell("edited", "3", 4),
                cell("stable", "4", 5),
                cell("same", "5", 5),
            ],
            da,
        );
        let b = run(
            "r_b",
            vec![
                cell("orders", "1b", 12),
                cell("summary", "2b", 1),
                cell("edited", "3b", 4),
                cell("stable", "4", 5),
                cell("same", "5", 5),
            ],
            db,
        );
        let cmp = compare(&a, &b);
        let by = |id: &str| cmp.cells.iter().find(|c| c.cell_id == id);
        assert_eq!(
            by("orders").unwrap().reasons,
            vec![ChangeReason::Parameters]
        );
        assert_eq!(by("summary").unwrap().reasons, vec![ChangeReason::Upstream]);
        assert_eq!(by("edited").unwrap().reasons, vec![ChangeReason::Source]);
        assert_eq!(by("stable").unwrap().data, Some(DataVerdict::Changed));
        assert!(by("same").is_none());
        assert_eq!(cmp.parameters.len(), 1);
        let text = render_comparison(&cmp, &BTreeMap::new());
        assert!(
            text.contains("orders: parameters changed; rows 10 -> 12"),
            "{text}"
        );
        assert!(text.contains("stable: same query, data changed"), "{text}");
        assert!(
            text.contains("parameter region: \"EU\" -> \"US\""),
            "{text}"
        );
    }

    #[test]
    fn incomplete_outputs_never_claim_unchanged_data() {
        let mut da = RunDetails::default();
        let mut db = RunDetails::default();
        da.outputs.insert(
            "c".into(),
            output(10, "x", Completeness::Truncated { cap: 10 }),
        );
        db.outputs.insert(
            "c".into(),
            output(10, "x", Completeness::Truncated { cap: 10 }),
        );
        assert_eq!(data_verdict(&da, &db, "c"), DataVerdict::Unknown);
        db.outputs
            .insert("c".into(), output(10, "x", Completeness::Complete));
        da.outputs
            .insert("c".into(), output(10, "x", Completeness::Complete));
        assert_eq!(data_verdict(&da, &db, "c"), DataVerdict::Unchanged);
    }

    #[test]
    fn assertion_and_kpi_changes_and_redacted_params() {
        let mut da = RunDetails::default();
        let mut db = RunDetails::default();
        da.assertions.insert(
            "checks".into(),
            serde_json::json!([{"name": "unique", "status": "pass"}]),
        );
        db.assertions.insert(
            "checks".into(),
            serde_json::json!([{"name": "unique", "status": "fail"}]),
        );
        da.kpis
            .insert("kpi".into(), serde_json::json!({"value": 10.0}));
        db.kpis
            .insert("kpi".into(), serde_json::json!({"value": 12.5}));
        for (details, value) in [(&mut da, "a"), (&mut db, "b")] {
            details.parameters.push(RedactedParam {
                name: "token".into(),
                type_tag: "text".into(),
                value: None,
                redacted: true,
            });
            let _ = value;
        }
        let a = run("r_a", vec![cell("checks", "1", 1), cell("kpi", "2", 1)], da);
        let b = run("r_b", vec![cell("checks", "1", 1), cell("kpi", "2", 1)], db);
        let cmp = compare(&a, &b);
        assert!(cmp.parameters.is_empty());
        let checks = cmp.cells.iter().find(|c| c.cell_id == "checks").unwrap();
        assert_eq!(checks.assertions, vec!["unique: pass -> fail".to_string()]);
        let kpi = cmp.cells.iter().find(|c| c.cell_id == "kpi").unwrap();
        assert_eq!(kpi.kpi, Some((Some(10.0), Some(12.5))));
    }
}
