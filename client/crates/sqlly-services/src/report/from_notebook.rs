//! A notebook run as a [`ReportDoc`]: the one builder the app's export
//! dialog and `sqlly notebook export` share.
//!
//! Cells become blocks in document order: Markdown cells → Markdown, SQL
//! cells → a heading, the SQL, the result table (with its completeness)
//! and a provenance block, chart cells → a chart over their input's
//! output, KPI cells → KPI tiles, assertion cells → one assertion row per
//! check, table and pivot cells → their input's table. Parameter, layout
//! and display-only kinds the report cannot show are left out. Templates
//! (`apply_template`) then reorder, cap and move SQL and provenance into
//! an appendix; `redact` runs last.

use std::collections::BTreeMap;

use serde_json::Value;
use sqlly_core::{DisplayMessage, ResultColumn};

use super::chart::ChartSpec;
use super::doc::{
    AssertionResult, AssertionStatus, Block, Callout, CalloutKind, ChartBlock, CodeBlock,
    ConnectionLabel, Heading, KpiBlock, KpiComparison, MarkdownBlock, Provenance, ReportDoc,
    ReportParameter, Status, TableBlock, TableColumn,
};
use super::labels::ReportLabels;
use crate::analytics::assertions::{AssertionOutcome, AssertionStatus as Checked};
use crate::analytics::kpi::{Direction, KpiResult, KpiStatus};
use crate::notebook::run_record::{RedactedParam, RunRecord};
use crate::notebook::{Cell, CellKind, CellOutput, NotebookDoc};

/// Everything the builder reads. Outputs, check outcomes and KPI results
/// are keyed by cell id; parameters arrive already redacted.
#[derive(Clone, Copy)]
pub struct NotebookReportInput<'a> {
    pub doc: &'a NotebookDoc,
    pub run: Option<&'a RunRecord>,
    pub outputs: &'a BTreeMap<String, CellOutput>,
    pub assertions: &'a BTreeMap<String, Vec<AssertionOutcome>>,
    pub kpis: &'a BTreeMap<String, KpiResult>,
    pub parameters: &'a [RedactedParam],
    pub connections: &'a [ConnectionLabel],
    /// RFC 3339, supplied so a render is reproducible.
    pub generated_at: Option<&'a str>,
    /// The words the builder writes itself (comparison names, captions).
    pub labels: &'a ReportLabels,
    /// Turns check issues into the reader's language.
    pub resolve_message: fn(&DisplayMessage) -> String,
}

fn table_of(cell_id: &str, output: &CellOutput) -> TableBlock {
    let columns = output
        .columns
        .iter()
        .map(|c: &ResultColumn| TableColumn::new(c.name.clone(), c.data_type.clone()))
        .collect();
    let mut table = TableBlock::new(columns, output.rows.as_ref().clone());
    table.cell_id = Some(cell_id.to_string());
    table.completeness = output.completeness.into();
    table.total_rows = Some(output.row_count);
    table
}

fn provenance_of(cell_id: &str, output: &CellOutput) -> Provenance {
    let p = &output.provenance;
    Provenance {
        cell_id: Some(cell_id.to_string()),
        run_id: Some(output.run_id.clone()),
        connection: (!p.connection_label.is_empty()).then(|| p.connection_label.clone()),
        database: p.database.clone(),
        environment: p.environment.clone(),
        extracted_at: p.extracted_at.map(|t| t.to_rfc3339()),
        sql_hash: p.sql_hash.clone(),
        row_count: Some(output.row_count),
    }
}

/// `"col"` → `{"column": "col"}` for the encoding channels, so a short
/// `{"kind": "line", "x": "day", "y": "total"}` spec reads as a
/// [`ChartSpec`].
fn normalize_chart_spec(spec: &Value) -> Value {
    let mut value = spec.clone();
    let Value::Object(map) = &mut value else {
        return value;
    };
    for key in ["input", "inputs"] {
        map.remove(key);
    }
    let encoding = |v: &Value| match v {
        Value::String(column) => serde_json::json!({ "column": column }),
        other => other.clone(),
    };
    for key in ["x", "series", "size", "color"] {
        if let Some(v) = map.get(key).map(encoding) {
            map.insert(key.to_string(), v);
        }
    }
    if let Some(y) = map.get("y").cloned() {
        let list = match y {
            Value::Array(items) => items.iter().map(encoding).collect(),
            other => vec![encoding(&other)],
        };
        map.insert("y".into(), Value::Array(list));
    }
    value
}

/// A chart cell's spec as a [`ChartSpec`], when it reads as one.
pub fn chart_spec(cell: &Cell) -> Option<ChartSpec> {
    serde_json::from_value(normalize_chart_spec(&cell.spec)).ok()
}

fn input_output<'a>(
    input: &NotebookReportInput<'a>,
    cell: &Cell,
) -> Option<(String, &'a CellOutput)> {
    let token = cell.spec_inputs().into_iter().next()?;
    let source = input
        .doc
        .cell(&token)
        .or_else(|| input.doc.resolve(&token))?;
    input
        .outputs
        .get(&source.id)
        .map(|o| (source.id.clone(), o))
}

fn kpi_status(status: KpiStatus) -> Option<Status> {
    match status {
        KpiStatus::Good => Some(Status::Good),
        KpiStatus::Warning => Some(Status::Warning),
        KpiStatus::Critical => Some(Status::Critical),
        KpiStatus::Neutral => Some(Status::Neutral),
        KpiStatus::Unknown => None,
    }
}

fn kpi_block(cell: &Cell, result: &KpiResult, labels: &ReportLabels) -> KpiBlock {
    let comparison_label = match cell
        .spec
        .pointer("/comparison/kind")
        .and_then(Value::as_str)
    {
        Some("target") => labels.kpi_target.clone(),
        Some("baseline") => labels.kpi_baseline.clone(),
        _ => result
            .comparison_period
            .clone()
            .unwrap_or_else(|| labels.kpi_previous.clone()),
    };
    let direction: Direction = cell
        .spec
        .get("direction")
        .and_then(|d| serde_json::from_value(d.clone()).ok())
        .unwrap_or_default();
    KpiBlock {
        cell_id: Some(cell.id.clone()),
        label: cell.relation_name().to_string(),
        value: result.value.is_finite().then_some(result.value),
        comparison: result.comparison_value.map(|value| KpiComparison {
            label: comparison_label,
            value,
        }),
        status: kpi_status(result.status),
        sparkline: result.sparkline.iter().map(|p| p.value).collect(),
        higher_is_better: direction != Direction::LowerIsBetter,
        caption: result.inconclusive.then(|| labels.kpi_incomplete.clone()),
        ..KpiBlock::default()
    }
}

fn observed_text(outcome: &AssertionOutcome) -> Option<String> {
    let observed: Vec<String> = outcome
        .evidence
        .observed
        .iter()
        .map(|(k, v)| {
            let v = serde_json::to_value(v).unwrap_or(Value::Null);
            match v {
                Value::String(s) => format!("{k} {s}"),
                other => format!("{k} {other}"),
            }
        })
        .collect();
    (!observed.is_empty()).then(|| observed.join(", "))
}

fn assertion_rows(
    input: &NotebookReportInput<'_>,
    cell: &Cell,
    outcomes: &[AssertionOutcome],
) -> Vec<Block> {
    let labels = input.labels;
    outcomes
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let status = match o.status {
                Checked::Pass => AssertionStatus::Passed,
                Checked::Fail => AssertionStatus::Failed,
                Checked::Warn => AssertionStatus::Warning,
                Checked::Error => AssertionStatus::Error,
                Checked::Inconclusive => AssertionStatus::Inconclusive,
            };
            let message = match (&o.issue, o.evidence.failing_count) {
                (Some(issue), _) => Some((input.resolve_message)(&issue.message())),
                (None, n) if n > 0 => Some(ReportLabels::fill(
                    if n == 1 {
                        &labels.failing_rows_one
                    } else {
                        &labels.failing_rows
                    },
                    &[("count", &n.to_string())],
                )),
                _ => None,
            };
            Block::Assertion(AssertionResult {
                cell_id: Some(cell.id.clone()),
                name: o.name.clone().unwrap_or_else(|| {
                    ReportLabels::fill(
                        &labels.check_name,
                        &[
                            ("cell", cell.relation_name()),
                            ("number", &(i + 1).to_string()),
                        ],
                    )
                }),
                status,
                expected: None,
                actual: observed_text(o),
                message,
            })
        })
        .collect()
}

/// Build the report for a notebook run.
pub fn from_notebook(input: &NotebookReportInput<'_>) -> ReportDoc {
    let doc = input.doc;
    let mut report = ReportDoc::default();
    report.meta.title = doc.title.clone();
    report.meta.notebook_id = Some(doc.notebook_id.clone());
    report.meta.generated_at = input.generated_at.map(str::to_string);
    if let Some(run) = input.run {
        report.meta.notebook_version = Some(run.doc_hash.clone());
        report.meta.subtitle = Some(ReportLabels::fill(
            &input.labels.run_subtitle,
            &[
                ("run", &run.run_id),
                (
                    "started",
                    &run.started_at.format("%Y-%m-%d %H:%M UTC").to_string(),
                ),
            ],
        ));
    }
    report.meta.parameters = input
        .parameters
        .iter()
        .map(|p| ReportParameter {
            name: p.name.clone(),
            value: match &p.value {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            },
            sensitive: p.redacted,
        })
        .collect();
    report.meta.connections = input.connections.to_vec();

    let errors: BTreeMap<&str, &str> = input
        .run
        .into_iter()
        .flat_map(|r| &r.cells)
        .filter_map(|c| {
            c.error
                .as_ref()
                .map(|e| (c.cell_id.as_str(), e.message.as_str()))
        })
        .collect();
    let blocks = &mut report.blocks;
    for cell in &doc.cells {
        let title = cell.relation_name().to_string();
        let heading = |blocks: &mut Vec<Block>| {
            blocks.push(Block::Heading(Heading {
                level: 2,
                text: title.clone(),
            }));
        };
        let failed = |blocks: &mut Vec<Block>| {
            if let Some(message) = errors.get(cell.id.as_str()) {
                blocks.push(Block::Callout(Callout {
                    kind: CalloutKind::Warning,
                    title: None,
                    text: (*message).to_string(),
                    evidence: input.run.map(|r| r.run_id.clone()).into_iter().collect(),
                }));
            }
        };
        match cell.kind {
            CellKind::Markdown => {
                if !cell.source.trim().is_empty() {
                    blocks.push(Block::Markdown(MarkdownBlock {
                        source: cell.source.clone(),
                    }));
                }
            }
            CellKind::Sql => {
                heading(blocks);
                if !cell.source.trim().is_empty() {
                    let mut code = CodeBlock::sql(cell.source.trim_end());
                    code.cell_id = Some(cell.id.clone());
                    blocks.push(Block::Code(code));
                }
                failed(blocks);
                if let Some(output) = input.outputs.get(&cell.id) {
                    if !output.columns.is_empty() {
                        blocks.push(Block::Table(table_of(&cell.id, output)));
                    }
                    blocks.push(Block::Provenance(provenance_of(&cell.id, output)));
                }
            }
            CellKind::Chart => {
                let (Some(spec), Some((source, output))) =
                    (chart_spec(cell), input_output(input, cell))
                else {
                    continue;
                };
                let mut data = table_of(&source, output);
                data.cell_id = Some(source);
                blocks.push(Block::Chart(ChartBlock {
                    cell_id: Some(cell.id.clone()),
                    spec,
                    data,
                    caption: None,
                }));
            }
            CellKind::Table | CellKind::Pivot => {
                if let Some((_, output)) = input_output(input, cell) {
                    heading(blocks);
                    let mut table = table_of(&cell.id, output);
                    table.title = Some(title.clone());
                    blocks.push(Block::Table(table));
                }
            }
            CellKind::Kpi => match input.kpis.get(&cell.id) {
                Some(result) => blocks.push(Block::Kpi(kpi_block(cell, result, input.labels))),
                None => failed(blocks),
            },
            CellKind::Assertion => match input.assertions.get(&cell.id) {
                Some(outcomes) => blocks.extend(assertion_rows(input, cell, outcomes)),
                None => failed(blocks),
            },
            CellKind::Ai => blocks.extend(
                crate::notebook::cells::ai::report_callouts(cell)
                    .into_iter()
                    .map(Block::Callout),
            ),
            _ => {}
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::fingerprint::Fingerprint;
    use crate::notebook::{Completeness, ExecutionPolicy, Provenance as SourceProvenance};
    use crate::report::{RenderOptions, ReportFormat, render};
    use serde_json::json;
    use sqlly_core::CellValue;

    fn cell(id: &str, kind: CellKind, name: &str, source: &str, spec: Value) -> Cell {
        let mut c = Cell::new(id, kind, ExecutionPolicy::default());
        c.name = Some(name.into());
        c.source = source.into();
        c.spec = spec;
        c
    }

    fn output(rows: Vec<Vec<CellValue>>, completeness: Completeness) -> CellOutput {
        let n = rows.len() as u64;
        CellOutput {
            columns: vec![
                ResultColumn {
                    name: "region".into(),
                    data_type: "text".into(),
                    nullable: true,
                },
                ResultColumn {
                    name: "total".into(),
                    data_type: "real".into(),
                    nullable: true,
                },
            ],
            rows: std::sync::Arc::new(rows),
            completeness,
            row_count: n,
            elapsed_ms: 3,
            run_id: "r_1".into(),
            fingerprint: Fingerprint("f".into()),
            provenance: SourceProvenance {
                connection_label: "Sales".into(),
                database: Some("sales".into()),
                ..SourceProvenance::default()
            },
            freshness: Default::default(),
        }
    }

    #[test]
    fn cells_become_blocks_in_order() {
        let mut doc = NotebookDoc::empty("nb", "Revenue");
        doc.cells = vec![
            cell("c_md", CellKind::Markdown, "intro", "# Weekly", Value::Null),
            cell(
                "c_sql",
                CellKind::Sql,
                "by_region",
                "SELECT region, total FROM t",
                Value::Null,
            ),
            cell(
                "c_chart",
                CellKind::Chart,
                "chart",
                "",
                json!({"input": "by_region", "kind": "bar", "x": "region", "y": "total"}),
            ),
            cell(
                "c_kpi",
                CellKind::Kpi,
                "revenue",
                "",
                json!({"input": "by_region", "comparison": {"kind": "target", "value": 10}}),
            ),
            cell("c_param", CellKind::Parameter, "p", "", Value::Null),
        ];
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "c_sql".to_string(),
            output(
                vec![
                    vec![CellValue::String("EU".into()), CellValue::F64(12.5)],
                    vec![CellValue::String("US".into()), CellValue::F64(4.0)],
                ],
                Completeness::Truncated { cap: 2 },
            ),
        );
        let spec: crate::analytics::kpi::KpiSpec = serde_json::from_value(json!({
            "value": {"kind": "aggregate", "column": "total", "aggregate": "sum"},
            "comparison": {"kind": "target", "value": 10.0}
        }))
        .unwrap();
        let source = &outputs["c_sql"];
        let view = crate::analytics::OutputView::complete(&source.columns, &source.rows);
        let kpi = crate::analytics::kpi::evaluate_kpi(&view, &spec, None).unwrap();
        let kpis = BTreeMap::from([("c_kpi".to_string(), kpi)]);
        let report = from_notebook(&NotebookReportInput {
            doc: &doc,
            run: None,
            outputs: &outputs,
            assertions: &BTreeMap::new(),
            kpis: &kpis,
            parameters: &[],
            connections: &[],
            generated_at: Some("2026-11-18T07:00:00Z"),
            labels: ReportLabels::english(),
            resolve_message: crate::report::english,
        });
        let kinds: Vec<&str> = report
            .blocks
            .iter()
            .map(|b| match b {
                Block::Markdown(_) => "markdown",
                Block::Heading(_) => "heading",
                Block::Code(_) => "code",
                Block::Table(_) => "table",
                Block::Provenance(_) => "provenance",
                Block::Chart(_) => "chart",
                Block::Kpi(_) => "kpi",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "markdown",
                "heading",
                "code",
                "table",
                "provenance",
                "chart",
                "kpi"
            ]
        );
        let Block::Table(table) = &report.blocks[3] else {
            panic!()
        };
        assert_eq!(
            table.completeness,
            super::super::doc::Completeness::Truncated { cap: 2 }
        );
        let Block::Kpi(tile) = &report.blocks[6] else {
            panic!()
        };
        assert_eq!(tile.comparison.as_ref().unwrap().label, "Target");
        assert_eq!(tile.status, Some(Status::Good));
        let html = render(&report, ReportFormat::Html, &RenderOptions::default()).unwrap();
        let text = String::from_utf8_lossy(&html.files[0].bytes);
        assert!(text.contains("Revenue") && text.contains("12.5"));
    }

    #[test]
    fn short_chart_specs_read_as_chart_specs() {
        let c = cell(
            "c",
            CellKind::Chart,
            "c",
            "",
            json!({"input": "x", "kind": "line", "x": "day", "y": ["a", "b"]}),
        );
        let spec = chart_spec(&c).unwrap();
        assert_eq!(spec.x.unwrap().column, "day");
        assert_eq!(spec.y.len(), 2);
        let bad = cell(
            "c",
            CellKind::Chart,
            "c",
            "",
            json!({"kind": "not_a_chart"}),
        );
        assert!(chart_spec(&bad).is_none());
    }

    fn marked(message: &DisplayMessage) -> String {
        format!("<{}>", message.key().unwrap_or_default())
    }

    #[test]
    fn builder_text_comes_from_the_labels_and_checks_keep_their_reason() {
        let mut doc = NotebookDoc::empty("nb", "Revenue");
        doc.cells = vec![
            cell("c_sql", CellKind::Sql, "by_region", "SELECT 1", Value::Null),
            cell(
                "c_kpi",
                CellKind::Kpi,
                "total",
                "",
                json!({"input": "c_sql", "comparison": {"kind": "target", "value": 10.0}}),
            ),
            cell(
                "c_check",
                CellKind::Assertion,
                "checks",
                "",
                json!({"input": "c_sql"}),
            ),
        ];
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "c_sql".to_string(),
            output(
                vec![vec![CellValue::String("EU".into()), CellValue::F64(12.5)]],
                Completeness::Truncated { cap: 1 },
            ),
        );
        let spec: crate::analytics::kpi::KpiSpec = serde_json::from_value(json!({
            "value": {"kind": "aggregate", "column": "total", "aggregate": "sum"},
            "comparison": {"kind": "target", "value": 10.0}
        }))
        .unwrap();
        let source = &outputs["c_sql"];
        let view = crate::analytics::OutputView::new(
            &source.columns,
            &source.rows,
            crate::analytics::Completeness::Truncated { cap: 1 },
        );
        let kpi = crate::analytics::kpi::evaluate_kpi(&view, &spec, None).unwrap();
        assert!(kpi.inconclusive);
        let kpis = BTreeMap::from([("c_kpi".to_string(), kpi)]);
        let assertions = BTreeMap::from([(
            "c_check".to_string(),
            vec![
                AssertionOutcome {
                    name: None,
                    status: Checked::Inconclusive,
                    issue: Some(
                        crate::analytics::assertions::AssertionIssue::IncompleteInput {
                            completeness: crate::analytics::Completeness::Truncated { cap: 1 },
                        },
                    ),
                    evidence: Default::default(),
                },
                AssertionOutcome {
                    name: None,
                    status: Checked::Fail,
                    issue: None,
                    evidence: crate::analytics::assertions::Evidence {
                        failing_count: 37,
                        ..Default::default()
                    },
                },
            ],
        )]);
        let labels = ReportLabels::resolve(|key| match key {
            "notebook.report.labels.kpi_target" => Some("Ziel".into()),
            "notebook.report.labels.kpi_incomplete" => Some("Unvollständig".into()),
            "notebook.report.labels.failing_rows" => Some("{count} fehlerhafte Zeilen".into()),
            "notebook.report.labels.check_name" => Some("{cell} Prüfung {number}".into()),
            "notebook.report.labels.assertion_inconclusive" => Some("Unentschieden".into()),
            _ => None,
        });
        let report = from_notebook(&NotebookReportInput {
            doc: &doc,
            run: None,
            outputs: &outputs,
            assertions: &assertions,
            kpis: &kpis,
            parameters: &[],
            connections: &[],
            generated_at: None,
            labels: &labels,
            resolve_message: marked,
        });
        let kpi = report
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Kpi(k) => Some(k),
                _ => None,
            })
            .unwrap();
        assert_eq!(kpi.comparison.as_ref().unwrap().label, "Ziel");
        assert_eq!(kpi.caption.as_deref(), Some("Unvollständig"));
        let checks: Vec<&AssertionResult> = report
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Assertion(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(checks[0].status, AssertionStatus::Inconclusive);
        assert_eq!(
            checks[0].message.as_deref(),
            Some("<notebook.analysis.assertion.issue.incomplete_input>")
        );
        assert_eq!(checks[0].name, "checks Prüfung 1");
        assert_eq!(checks[1].message.as_deref(), Some("37 fehlerhafte Zeilen"));

        let options = RenderOptions {
            labels: labels.clone(),
            ..RenderOptions::default()
        };
        let md = render(&report, ReportFormat::Markdown, &options).unwrap();
        let md = String::from_utf8(md.files[0].bytes.clone()).unwrap();
        assert!(
            md.contains("Unvollständig"),
            "the KPI caption survives Markdown"
        );
        assert!(md.contains("Unentschieden"));
        assert!(md.contains("incomplete\\_input") || md.contains("incomplete_input"));
        let xlsx = render(&report, ReportFormat::Xlsx, &options).unwrap();
        let summary = {
            use calamine::Reader;
            let mut book =
                calamine::Xlsx::new(std::io::Cursor::new(xlsx.files[0].bytes.clone())).unwrap();
            let name = book.sheet_names()[0].clone();
            book.worksheet_range(&name).unwrap()
        };
        let cells: Vec<String> = summary.cells().map(|(_, _, v)| v.to_string()).collect();
        assert!(cells.iter().any(|c| c == "Unvollständig"), "{cells:?}");
        assert!(
            cells.iter().any(|c| c.contains("incomplete_input")),
            "{cells:?}"
        );
    }
}
