//! Report templates (spec 8.3): each rearranges a [`ReportDoc`] for a
//! reader (cover, ordering, how much data, where SQL and provenance go)
//! without inventing content. Blocks keep their relative order inside each
//! group the template pulls forward.

use serde::{Deserialize, Serialize};

use super::doc::{
    AssertionStatus, Block, CalloutKind, Column, Columns, Heading, Placement, ReportDoc,
    ReportLayout, Section,
};
use super::format::{ColumnKind, NumberStyle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateKind {
    /// Headline numbers and conclusions first; short tables; no SQL.
    Executive,
    /// What broke and why: warnings, failed checks and hypotheses first,
    /// full tables, SQL inline, provenance in the appendix.
    TechnicalIncident,
    /// Everything, in order, with SQL and provenance appendices and no row
    /// caps, so the report can be checked against its sources.
    Audit,
    /// KPIs, then tables with money formatting, SQL in the appendix.
    Financial,
    /// Status at a glance: KPIs and checks, then charts, short tables.
    Operational,
}

impl TemplateKind {
    pub const ALL: [TemplateKind; 5] = [
        TemplateKind::Executive,
        TemplateKind::TechnicalIncident,
        TemplateKind::Audit,
        TemplateKind::Financial,
        TemplateKind::Operational,
    ];

    pub fn name(self) -> &'static str {
        match self {
            TemplateKind::Executive => "executive",
            TemplateKind::TechnicalIncident => "technical_incident",
            TemplateKind::Audit => "audit",
            TemplateKind::Financial => "financial",
            TemplateKind::Operational => "operational",
        }
    }

    /// The catalog key for the template's display name.
    pub fn label_key(self) -> &'static str {
        match self {
            TemplateKind::Executive => "notebook.report.templates.executive",
            TemplateKind::TechnicalIncident => "notebook.report.templates.technical_incident",
            TemplateKind::Audit => "notebook.report.templates.audit",
            TemplateKind::Financial => "notebook.report.templates.financial",
            TemplateKind::Operational => "notebook.report.templates.operational",
        }
    }

    pub fn layout(self) -> ReportLayout {
        let base = ReportLayout::default();
        match self {
            TemplateKind::Executive => ReportLayout {
                sql: Placement::Omit,
                provenance: Placement::Omit,
                table_row_cap: Some(20),
                ..base
            },
            TemplateKind::TechnicalIncident => ReportLayout {
                sql: Placement::Inline,
                provenance: Placement::Appendix,
                table_row_cap: Some(200),
                ..base
            },
            TemplateKind::Audit => ReportLayout {
                sql: Placement::Appendix,
                provenance: Placement::Appendix,
                table_row_cap: None,
                ..base
            },
            TemplateKind::Financial => ReportLayout {
                sql: Placement::Appendix,
                provenance: Placement::Appendix,
                table_row_cap: Some(500),
                ..base
            },
            TemplateKind::Operational => ReportLayout {
                sql: Placement::Omit,
                provenance: Placement::Appendix,
                table_row_cap: Some(50),
                contents: false,
                ..base
            },
        }
    }
}

/// Flattens sections and columns into one list, keeping order.
fn flatten(blocks: Vec<Block>) -> Vec<Block> {
    let mut out = Vec::new();
    for block in blocks {
        match block {
            Block::Section(s) => {
                if let Some(title) = s.title {
                    out.push(Block::Heading(Heading {
                        level: 2,
                        text: title,
                    }));
                }
                out.extend(flatten(s.blocks));
            }
            Block::Columns(c) => {
                for column in c.columns {
                    out.extend(flatten(column.blocks));
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// KPI cards side by side, four to a row on the 12-column grid.
fn kpi_rows(kpis: Vec<Block>) -> Vec<Block> {
    kpis.chunks(4)
        .map(|chunk| {
            let span = (12 / chunk.len().max(1)) as u8;
            Block::Columns(Columns {
                columns: chunk
                    .iter()
                    .map(|k| Column {
                        span,
                        blocks: vec![k.clone()],
                    })
                    .collect(),
            })
        })
        .collect()
}

fn take(blocks: &mut Vec<Block>, pred: impl Fn(&Block) -> bool) -> Vec<Block> {
    let (taken, rest): (Vec<Block>, Vec<Block>) =
        std::mem::take(blocks).into_iter().partition(|b| pred(b));
    *blocks = rest;
    taken
}

fn is_kpi(b: &Block) -> bool {
    matches!(b, Block::Kpi(_))
}

fn is_callout(b: &Block, kinds: &[CalloutKind]) -> bool {
    matches!(b, Block::Callout(c) if kinds.contains(&c.kind))
}

fn is_problem(b: &Block) -> bool {
    matches!(b, Block::Assertion(a) if matches!(a.status, AssertionStatus::Failed | AssertionStatus::Error | AssertionStatus::Warning))
}

/// The document arranged for `kind`. Content is never added or dropped
/// here (SQL and provenance placement is a layout choice the renderers
/// apply), except that headings left with nothing under them are kept.
pub fn apply_template(doc: &ReportDoc, kind: TemplateKind) -> ReportDoc {
    let mut out = doc.clone();
    out.meta.template = Some(kind);
    out.meta.layout = kind.layout();
    let mut rest = std::mem::take(&mut out.blocks);
    let blocks = match kind {
        TemplateKind::Executive => {
            rest = flatten(rest);
            let kpis = take(&mut rest, is_kpi);
            let headline = take(&mut rest, |b| {
                is_callout(
                    b,
                    &[
                        CalloutKind::Recommendation,
                        CalloutKind::Fact,
                        CalloutKind::Warning,
                    ],
                )
            });
            let mut blocks = kpi_rows(kpis);
            blocks.extend(headline);
            blocks.extend(rest);
            blocks
        }
        TemplateKind::TechnicalIncident => {
            rest = flatten(rest);
            let warnings = take(&mut rest, |b| is_callout(b, &[CalloutKind::Warning]));
            let failures = take(&mut rest, is_problem);
            let hypotheses = take(&mut rest, |b| {
                is_callout(b, &[CalloutKind::Hypothesis, CalloutKind::Derived])
            });
            let mut blocks = warnings;
            blocks.extend(failures);
            blocks.extend(hypotheses);
            blocks.extend(rest);
            blocks
        }
        TemplateKind::Audit => rest,
        TemplateKind::Financial => {
            rest = flatten(rest);
            let kpis = take(&mut rest, is_kpi);
            for block in &mut rest {
                if let Block::Table(t) = block {
                    for column in &mut t.columns {
                        if column.kind() == ColumnKind::Decimal && column.format.is_default() {
                            column.format.decimals = Some(2);
                            column.format.thousands = true;
                        } else if column.kind().is_numeric()
                            && column.format.style == NumberStyle::Plain
                        {
                            column.format.thousands = true;
                        }
                    }
                }
            }
            let mut blocks = kpi_rows(kpis);
            blocks.extend(rest);
            blocks
        }
        TemplateKind::Operational => {
            rest = flatten(rest);
            let kpis = take(&mut rest, is_kpi);
            let checks = take(&mut rest, |b| matches!(b, Block::Assertion(_)));
            let charts = take(&mut rest, |b| matches!(b, Block::Chart(_)));
            let mut blocks = kpi_rows(kpis);
            if !checks.is_empty() {
                blocks.push(Block::Section(Section {
                    title: None,
                    blocks: checks,
                }));
            }
            blocks.extend(charts);
            blocks.extend(rest);
            blocks
        }
    };
    out.blocks = blocks;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::doc::*;

    fn doc() -> ReportDoc {
        ReportDoc {
            meta: ReportMeta {
                title: "Ops".into(),
                ..Default::default()
            },
            blocks: vec![
                Block::Heading(Heading {
                    level: 1,
                    text: "Intro".into(),
                }),
                Block::Code(CodeBlock::sql("select 1")),
                Block::Kpi(KpiBlock {
                    label: "A".into(),
                    value: Some(1.0),
                    ..Default::default()
                }),
                Block::Assertion(AssertionResult {
                    cell_id: None,
                    name: "rows > 0".into(),
                    status: AssertionStatus::Failed,
                    expected: None,
                    actual: None,
                    message: None,
                }),
                Block::Callout(Callout {
                    kind: CalloutKind::Warning,
                    text: "Late data".into(),
                    ..Default::default()
                }),
                Block::Kpi(KpiBlock {
                    label: "B".into(),
                    value: Some(2.0),
                    ..Default::default()
                }),
            ],
        }
    }

    #[test]
    fn executive_leads_with_kpis_and_drops_sql_from_the_layout() {
        let out = apply_template(&doc(), TemplateKind::Executive);
        assert!(
            matches!(&out.blocks[0], Block::Columns(c) if c.columns.len() == 2 && c.columns[0].span == 6)
        );
        assert!(matches!(&out.blocks[1], Block::Callout(_)));
        assert_eq!(out.meta.layout.sql, Placement::Omit);
        assert_eq!(out.meta.template, Some(TemplateKind::Executive));
    }

    #[test]
    fn incident_leads_with_problems() {
        let out = apply_template(&doc(), TemplateKind::TechnicalIncident);
        assert!(matches!(&out.blocks[0], Block::Callout(c) if c.kind == CalloutKind::Warning));
        assert!(matches!(&out.blocks[1], Block::Assertion(_)));
        assert_eq!(out.meta.layout.provenance, Placement::Appendix);
    }

    #[test]
    fn audit_keeps_order_and_every_row() {
        let out = apply_template(&doc(), TemplateKind::Audit);
        assert_eq!(out.blocks, doc().blocks);
        assert_eq!(out.meta.layout.table_row_cap, None);
        assert_eq!(out.meta.layout.sql, Placement::Appendix);
    }

    #[test]
    fn every_template_keeps_every_block() {
        let count = |d: &ReportDoc| {
            let mut n = 0;
            walk(&d.blocks, &mut |b| {
                if !matches!(b, Block::Columns(_) | Block::Section(_)) {
                    n += 1;
                }
            });
            n
        };
        for kind in TemplateKind::ALL {
            assert_eq!(
                count(&apply_template(&doc(), kind)),
                count(&doc()),
                "{kind:?}"
            );
        }
    }
}
