//! The redaction pass every export runs before rendering (spec 12.2):
//! sensitive parameters go, sensitive columns go from tables and charts,
//! a structure-only export drops every row and value, and any text that
//! quotes a sensitive parameter value has it replaced.

use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

use super::doc::{Block, CalloutKind, Completeness, ReportDoc, TableBlock, walk_mut};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionPolicy {
    /// Remove parameters flagged sensitive (and their values anywhere in
    /// the report text).
    #[serde(default = "yes")]
    pub sensitive_parameters: bool,
    /// Remove columns flagged sensitive.
    #[serde(default = "yes")]
    pub sensitive_columns: bool,
    /// More column names to treat as sensitive (case-insensitive).
    #[serde(default)]
    pub extra_sensitive_columns: Vec<String>,
    /// Structure-only export: no rows, KPI values or images.
    #[serde(default)]
    pub drop_data: bool,
    /// Leave SQL text out.
    #[serde(default)]
    pub drop_sql: bool,
    /// Replacement text for redacted values in prose.
    #[serde(default = "redacted")]
    pub replacement: String,
}

fn yes() -> bool {
    true
}

fn redacted() -> String {
    "[redacted]".into()
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        Self {
            sensitive_parameters: true,
            sensitive_columns: true,
            extra_sensitive_columns: Vec::new(),
            drop_data: false,
            drop_sql: false,
            replacement: redacted(),
        }
    }
}

impl RedactionPolicy {
    /// No row data leaves the machine: the data permission is "structure".
    pub fn structure_only() -> Self {
        Self {
            drop_data: true,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionSummary {
    pub parameters_removed: usize,
    pub columns_removed: usize,
    pub charts_removed: usize,
    pub rows_removed: u64,
    pub values_replaced: usize,
    pub sql_removed: usize,
    pub images_removed: usize,
    /// Fact, derived and evidence-backed callouts left out of a
    /// structure-only export, since their text quotes the data.
    pub callouts_removed: usize,
}

/// Shortest sensitive value searched for in prose; shorter ones would
/// match ordinary words.
const MIN_SECRET_LEN: usize = 4;

pub fn redact(doc: &ReportDoc, policy: &RedactionPolicy) -> (ReportDoc, RedactionSummary) {
    let mut out = doc.clone();
    let mut summary = RedactionSummary::default();
    let mut secrets: Vec<String> = Vec::new();
    if policy.sensitive_parameters {
        out.meta.parameters.retain(|p| {
            if p.sensitive {
                summary.parameters_removed += 1;
                if p.value.trim().len() >= MIN_SECRET_LEN {
                    secrets.push(p.value.trim().to_string());
                }
                false
            } else {
                true
            }
        });
    }
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    if policy.drop_data {
        out.meta.data_withheld = true;
    }
    let extra: Vec<String> = policy
        .extra_sensitive_columns
        .iter()
        .map(|c| c.to_lowercase())
        .collect();
    let is_sensitive = |name: &str, flagged: bool| {
        (policy.sensitive_columns && flagged) || extra.contains(&name.to_lowercase())
    };
    let scrub = |text: &mut String, summary: &mut RedactionSummary| {
        for secret in &secrets {
            let count = text.matches(secret.as_str()).count();
            if count > 0 {
                *text = text.replace(secret.as_str(), &policy.replacement);
                summary.values_replaced += count;
            }
        }
    };
    let drop_columns = |table: &mut TableBlock, summary: &mut RedactionSummary| -> Vec<String> {
        let removed: Vec<usize> = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| is_sensitive(&c.name, c.sensitive))
            .map(|(i, _)| i)
            .collect();
        let names = removed
            .iter()
            .map(|&i| table.columns[i].name.clone())
            .collect();
        for &i in removed.iter().rev() {
            table.columns.remove(i);
            for row in &mut table.rows {
                if i < row.len() {
                    row.remove(i);
                }
            }
        }
        summary.columns_removed += removed.len();
        names
    };
    walk_mut(&mut out.blocks, &mut |block| match block {
        Block::Table(table) => {
            drop_columns(table, &mut summary);
            if policy.drop_data {
                withhold(table, &mut summary);
            }
            for text in [&mut table.title, &mut table.caption].into_iter().flatten() {
                scrub(text, &mut summary);
            }
        }
        Block::Chart(chart) => {
            let removed = drop_columns(&mut chart.data, &mut summary);
            let uses_removed = chart
                .spec
                .columns()
                .iter()
                .any(|c| removed.iter().any(|r| r.eq_ignore_ascii_case(c)));
            if uses_removed {
                chart.data.rows.clear();
                chart.data.data_withheld = true;
                summary.charts_removed += 1;
            }
            if policy.drop_data {
                withhold(&mut chart.data, &mut summary);
            }
            for text in [
                &mut chart.spec.title,
                &mut chart.spec.subtitle,
                &mut chart.caption,
            ]
            .into_iter()
            .flatten()
            {
                scrub(text, &mut summary);
            }
        }
        Block::Kpi(kpi) => {
            if policy.drop_data {
                kpi.value = None;
                kpi.comparison = None;
                kpi.sparkline.clear();
                kpi.status = None;
                kpi.thresholds.clear();
                kpi.caption = None;
            }
            scrub(&mut kpi.label, &mut summary);
        }
        Block::Assertion(a) => {
            if policy.drop_data {
                // The message carries counts ("37 failing rows") and values.
                a.actual = None;
                a.expected = None;
                a.message = None;
            }
            for text in [&mut a.actual, &mut a.expected, &mut a.message]
                .into_iter()
                .flatten()
            {
                scrub(text, &mut summary);
            }
        }
        Block::Markdown(m) => scrub(&mut m.source, &mut summary),
        Block::Heading(h) => scrub(&mut h.text, &mut summary),
        Block::Callout(c) => {
            scrub(&mut c.text, &mut summary);
            if let Some(t) = &mut c.title {
                scrub(t, &mut summary);
            }
        }
        Block::Code(code) => {
            if policy.drop_sql {
                code.source.clear();
                summary.sql_removed += 1;
            } else {
                scrub(&mut code.source, &mut summary);
            }
        }
        Block::Image(_) => {}
        Block::Provenance(p) => {
            if policy.drop_data {
                p.row_count = None;
            }
        }
        Block::Section(s) => {
            if let Some(t) = &mut s.title {
                scrub(t, &mut summary);
            }
        }
        Block::PageBreak | Block::Columns(_) => {}
    });
    if policy.drop_data || policy.drop_sql {
        let drop_data = policy.drop_data;
        let drop_sql = policy.drop_sql;
        retain_blocks(&mut out.blocks, &mut |b| match b {
            Block::Image(_) if drop_data => {
                summary.images_removed += 1;
                false
            }
            Block::Callout(c)
                if drop_data
                    && (matches!(c.kind, CalloutKind::Fact | CalloutKind::Derived)
                        || !c.evidence.is_empty()) =>
            {
                summary.callouts_removed += 1;
                false
            }
            Block::Code(c) if drop_sql && c.source.is_empty() => false,
            _ => true,
        });
    }
    scrub(&mut out.meta.title, &mut summary);
    if let Some(s) = &mut out.meta.subtitle {
        scrub(s, &mut summary);
    }
    (out, summary)
}

/// Rows dropped, and with them every count that describes them (the row
/// total and the truncated/sampled badge), so nothing says how much data
/// there was.
fn withhold(table: &mut TableBlock, summary: &mut RedactionSummary) {
    summary.rows_removed += table.rows.len() as u64;
    table.rows.clear();
    table.data_withheld = true;
    table.total_rows = None;
    table.completeness = Completeness::Complete;
}

fn retain_blocks(blocks: &mut Vec<Block>, keep: &mut dyn FnMut(&Block) -> bool) {
    blocks.retain(|b| keep(b));
    for block in blocks.iter_mut() {
        match block {
            Block::Section(s) => retain_blocks(&mut s.blocks, keep),
            Block::Columns(c) => {
                for column in &mut c.columns {
                    retain_blocks(&mut column.blocks, keep);
                }
            }
            _ => {}
        }
    }
}

/// True when the document still holds a sensitive parameter, a sensitive
/// column, or (for structure-only) any row; the export path asserts this is
/// false after [`redact`].
pub fn leaks(doc: &ReportDoc, policy: &RedactionPolicy) -> bool {
    let mut leak = policy.sensitive_parameters && doc.meta.parameters.iter().any(|p| p.sensitive);
    super::doc::walk(&doc.blocks, &mut |b| {
        let table = match b {
            Block::Table(t) => Some(t),
            Block::Chart(c) => Some(&c.data),
            Block::Kpi(k) if policy.drop_data => {
                leak |= k.value.is_some()
                    || k.comparison.is_some()
                    || k.status.is_some()
                    || k.caption.is_some();
                None
            }
            Block::Assertion(a) if policy.drop_data => {
                leak |= a.actual.is_some() || a.expected.is_some() || a.message.is_some();
                None
            }
            Block::Callout(c) if policy.drop_data => {
                leak |= matches!(c.kind, CalloutKind::Fact | CalloutKind::Derived)
                    || !c.evidence.is_empty();
                None
            }
            _ => None,
        };
        if let Some(t) = table {
            leak |= policy.sensitive_columns && t.columns.iter().any(|c| c.sensitive);
            leak |= policy.drop_data
                && (t.total_rows.is_some()
                    || t.completeness != Completeness::Complete
                    || t.rows
                        .iter()
                        .any(|r| r.iter().any(|v| *v != CellValue::Null)));
        }
    });
    leak
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::chart::{ChartKind, ChartSpec};
    use crate::report::doc::*;

    fn doc() -> ReportDoc {
        let mut table = TableBlock::new(
            vec![
                TableColumn::new("email", "varchar"),
                TableColumn::new("orders", "int"),
            ],
            vec![vec![
                CellValue::String("a@example.com".into()),
                CellValue::I64(3),
            ]],
        );
        table.columns[0].sensitive = true;
        let mut chart = ChartBlock {
            spec: ChartSpec::new(ChartKind::Bar).x("email").y("orders"),
            data: table.clone(),
            ..Default::default()
        };
        chart.spec.title = Some("Orders for tenant-4242".into());
        ReportDoc {
            meta: ReportMeta {
                title: "Tenant tenant-4242".into(),
                parameters: vec![
                    ReportParameter {
                        name: "tenant".into(),
                        value: "tenant-4242".into(),
                        sensitive: true,
                    },
                    ReportParameter {
                        name: "region".into(),
                        value: "West".into(),
                        sensitive: false,
                    },
                ],
                ..Default::default()
            },
            blocks: vec![
                Block::Markdown(MarkdownBlock {
                    source: "Numbers for tenant-4242 below.".into(),
                }),
                Block::Table(table),
                Block::Chart(chart),
                Block::Code(CodeBlock::sql(
                    "select * from t where tenant = 'tenant-4242'",
                )),
                Block::Kpi(KpiBlock {
                    label: "Orders".into(),
                    value: Some(3.0),
                    ..Default::default()
                }),
            ],
        }
    }

    #[test]
    fn sensitive_parameters_and_columns_are_removed_everywhere() {
        let policy = RedactionPolicy::default();
        let (out, summary) = redact(&doc(), &policy);
        assert_eq!(out.meta.parameters.len(), 1);
        assert_eq!(summary.parameters_removed, 1);
        assert_eq!(summary.columns_removed, 2);
        assert_eq!(summary.charts_removed, 1);
        let json = serde_json::to_string(&out).unwrap();
        assert!(!json.contains("tenant-4242"), "{json}");
        assert!(!json.contains("a@example.com"), "{json}");
        assert!(!leaks(&out, &policy));
        assert!(leaks(&doc(), &policy));
    }

    #[test]
    fn structure_only_drops_every_value() {
        let policy = RedactionPolicy::structure_only();
        let (out, summary) = redact(&doc(), &policy);
        assert!(out.meta.data_withheld);
        assert_eq!(summary.rows_removed, 1);
        let Block::Table(t) = &out.blocks[1] else {
            panic!()
        };
        assert!(t.rows.is_empty() && t.data_withheld);
        assert_eq!(t.columns.len(), 1);
        let Block::Kpi(k) = &out.blocks[4] else {
            panic!()
        };
        assert_eq!(k.value, None);
        assert!(!leaks(&out, &policy));
    }

    #[test]
    fn structure_only_drops_counts_statuses_and_quoted_figures() {
        let mut table = TableBlock::new(
            vec![TableColumn::new("orders", "int")],
            vec![vec![CellValue::I64(3)]],
        );
        table.total_rows = Some(9_000);
        table.completeness = Completeness::Truncated { cap: 1 };
        let doc = ReportDoc {
            blocks: vec![
                Block::Table(table),
                Block::Assertion(AssertionResult {
                    cell_id: None,
                    name: "unique ids".into(),
                    status: AssertionStatus::Failed,
                    expected: None,
                    actual: Some("failing_count 37".into()),
                    message: Some("37 failing rows".into()),
                }),
                Block::Kpi(KpiBlock {
                    label: "Revenue".into(),
                    value: Some(1.0),
                    status: Some(Status::Critical),
                    caption: Some("Computed from incomplete data; status withheld.".into()),
                    ..KpiBlock::default()
                }),
                Block::Callout(Callout {
                    kind: CalloutKind::Fact,
                    title: None,
                    text: "Revenue fell 12% to $4,210.".into(),
                    evidence: vec!["r_1".into()],
                }),
                Block::Callout(Callout {
                    kind: CalloutKind::Note,
                    title: None,
                    text: "Read with the Q3 notes.".into(),
                    evidence: Vec::new(),
                }),
            ],
            ..ReportDoc::default()
        };
        let policy = RedactionPolicy::structure_only();
        assert!(leaks(&doc, &policy));
        let (out, summary) = redact(&doc, &policy);
        assert!(!leaks(&out, &policy));
        assert_eq!(summary.callouts_removed, 1);
        let Block::Table(t) = &out.blocks[0] else {
            panic!()
        };
        assert_eq!(
            (t.total_rows, t.completeness),
            (None, Completeness::Complete)
        );
        let Block::Assertion(a) = &out.blocks[1] else {
            panic!()
        };
        assert_eq!(a.message, None);
        let Block::Kpi(k) = &out.blocks[2] else {
            panic!()
        };
        assert_eq!((k.status, k.caption.as_deref()), (None, None));
        assert!(matches!(&out.blocks[3], Block::Callout(c) if c.kind == CalloutKind::Note));
        let html = crate::report::render(
            &out,
            crate::report::ReportFormat::Html,
            &crate::report::RenderOptions::default(),
        )
        .unwrap();
        let html = String::from_utf8(html.files[0].bytes.clone()).unwrap();
        for figure in ["9,000", "9000", "Truncated", "37", "4,210", "Off track"] {
            assert!(!html.contains(figure), "{figure} leaked");
        }
    }

    #[test]
    fn sql_can_be_left_out() {
        let policy = RedactionPolicy {
            drop_sql: true,
            ..Default::default()
        };
        let (out, summary) = redact(&doc(), &policy);
        assert_eq!(summary.sql_removed, 1);
        assert!(!out.blocks.iter().any(|b| matches!(b, Block::Code(_))));
    }
}
