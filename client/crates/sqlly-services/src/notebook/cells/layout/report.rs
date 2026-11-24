//! A dashboard as a [`ReportDoc`]: each section tab becomes a report
//! section, each grid row a [`Columns`] block with the tiles' spans, so the
//! layout carries into HTML (CSS grid) and PDF (KPI rows side by side).
//!
//! A tile's blocks are what [`from_notebook`] makes for its cell (the
//! result table, the chart, the KPI, …) under the tile's title, followed by
//! the SQL and provenance of the cells it reads, which the report layout
//! then keeps inline, moves to the appendix or drops. Hidden tiles (their
//! visibility rule does not hold) are left out.

use std::collections::BTreeSet;

use super::{Breakpoint, LayoutSpec, Tile, VisibilityInputs, pack_rows};
use crate::analytics::assertions::AssertionStatus as Checked;
use crate::notebook::{Cell, CellKind, NotebookDoc};
use crate::report::doc::{
    Block, Column, Columns, Heading, ImageBlock, ImageData, MarkdownBlock, ReportDoc, ReportMeta,
    Section,
};
use crate::report::{NotebookReportInput, from_notebook};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DashboardReportOptions {
    /// Start every section on a new page (PDF).
    pub page_per_section: bool,
}

fn emit(input: &NotebookReportInput<'_>, cells: Vec<Cell>) -> Vec<Block> {
    let mut doc: NotebookDoc = input.doc.clone();
    doc.cells = cells;
    from_notebook(&NotebookReportInput {
        doc: &doc,
        ..*input
    })
    .blocks
}

/// The report's metadata (title, run, parameters, connections) without
/// any cell blocks.
pub fn report_meta(input: &NotebookReportInput<'_>) -> ReportMeta {
    let mut doc: NotebookDoc = input.doc.clone();
    doc.cells.clear();
    from_notebook(&NotebookReportInput {
        doc: &doc,
        ..*input
    })
    .meta
}

fn direct_inputs<'a>(doc: &'a NotebookDoc, cell: &Cell) -> Vec<&'a Cell> {
    let mut out: Vec<&Cell> = Vec::new();
    for token in cell.spec_inputs() {
        if let Some(found) = doc.cell(&token).or_else(|| doc.resolve(&token))
            && found.id != cell.id
            && !out.iter().any(|c| c.id == found.id)
        {
            out.push(found);
        }
    }
    out
}

/// The blocks [`from_notebook`] makes for one cell, exactly as it would
/// inside the whole notebook.
pub fn cell_blocks(input: &NotebookReportInput<'_>, cell_id: &str) -> Vec<Block> {
    let Some(cell) = input.doc.cell(cell_id) else {
        return Vec::new();
    };
    let deps: Vec<Cell> = direct_inputs(input.doc, cell)
        .into_iter()
        .cloned()
        .collect();
    let mut with = vec![cell.clone()];
    with.extend(deps.iter().cloned());
    let mut all = emit(input, with);
    let tail = if deps.is_empty() {
        0
    } else {
        emit(input, deps).len()
    };
    all.truncate(all.len().saturating_sub(tail));
    all
}

/// The SQL and provenance of the SQL cells `cell` reads from, through its
/// inputs, nearest first.
fn upstream_sources(input: &NotebookReportInput<'_>, cell: &Cell) -> Vec<Block> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::from([cell.id.clone()]);
    let mut queue: Vec<&Cell> = direct_inputs(input.doc, cell);
    while let Some(next) = queue.first().copied() {
        queue.remove(0);
        if !seen.insert(next.id.clone()) {
            continue;
        }
        if next.kind == CellKind::Sql {
            out.extend(
                cell_blocks(input, &next.id)
                    .into_iter()
                    .filter(|b| matches!(b, Block::Code(_) | Block::Provenance(_))),
            );
        }
        queue.extend(direct_inputs(input.doc, next));
    }
    out
}

/// The blocks of one tile's grid column.
pub fn tile_blocks(input: &NotebookReportInput<'_>, tile: &Tile) -> Vec<Block> {
    let Some(cell) = input.doc.cell(&tile.cell) else {
        return Vec::new();
    };
    let mut blocks = cell_blocks(input, &cell.id);
    if matches!(blocks.first(), Some(Block::Heading(_))) {
        blocks.remove(0);
    }
    let title = tile
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| (cell.kind != CellKind::Markdown).then(|| cell.relation_name().to_string()));
    let mut out = Vec::new();
    if let Some(text) = title {
        out.push(Block::Heading(Heading { level: 3, text }));
    }
    out.extend(blocks);
    out.extend(upstream_sources(input, cell));
    if let Some(note) = tile.annotation.as_deref().filter(|n| !n.trim().is_empty()) {
        out.push(Block::Markdown(MarkdownBlock {
            source: note.to_string(),
        }));
    }
    out
}

/// Parameter values as the dashboard reads them: each declared
/// parameter's current value as text.
pub fn param_text(doc: &NotebookDoc, name: &str) -> Option<String> {
    let def = doc.parameters.iter().find(|p| p.name == name)?;
    match def.default.as_ref()? {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Whether every check of an assertion cell in `input` passed (warnings
/// count as passing); `None` when the cell has no outcomes.
pub fn test_passed(input: &NotebookReportInput<'_>, cell: &str) -> Option<bool> {
    let outcomes = input.assertions.get(cell)?;
    Some(
        outcomes
            .iter()
            .all(|o| matches!(o.status, Checked::Pass | Checked::Warn)),
    )
}

/// The dashboard in layout cell `layout_cell` as a report, or `None` when
/// the cell is not a readable dashboard.
pub fn dashboard_report(
    input: &NotebookReportInput<'_>,
    layout_cell: &str,
    options: DashboardReportOptions,
) -> Option<ReportDoc> {
    let cell = input
        .doc
        .cell(layout_cell)
        .filter(|c| c.kind == CellKind::Layout)?;
    let spec = LayoutSpec::from_value(&cell.spec).ok()?;
    let param = |name: &str| param_text(input.doc, name);
    let test = |cell: &str| test_passed(input, cell);
    let visibility = VisibilityInputs {
        param: &param,
        test_passed: &test,
    };
    let mut report = ReportDoc {
        meta: report_meta(input),
        blocks: Vec::new(),
    };
    if !spec.title.trim().is_empty() {
        report.meta.title = spec.title.clone();
    }
    if let Some(logo) = &spec.logo
        && let Some(bytes) = logo.bytes()
    {
        let image = match logo.format {
            super::LogoFormat::Png => ImageData::Png { bytes },
            super::LogoFormat::Svg => ImageData::Svg {
                markup: logo.data.clone(),
            },
        };
        report.blocks.push(Block::Image(ImageBlock {
            alt: logo.alt.clone(),
            caption: None,
            image,
        }));
    }
    let mut first = true;
    for section in &spec.sections {
        let mut tabs = Vec::new();
        for tab in &section.tabs {
            let shown: Vec<&Tile> = tab
                .tiles
                .iter()
                .filter(|t| t.is_visible(&visibility) && input.doc.cell(&t.cell).is_some())
                .collect();
            let rows: Vec<Block> = pack_rows(&shown, Breakpoint::Wide)
                .into_iter()
                .map(|row| {
                    Block::Columns(Columns {
                        columns: row
                            .into_iter()
                            .map(|i| Column {
                                span: shown[i].span(Breakpoint::Wide),
                                blocks: tile_blocks(input, shown[i]),
                            })
                            .collect(),
                    })
                })
                .collect();
            tabs.push((tab.title.clone(), rows));
        }
        let blocks = if tabs.len() == 1 {
            tabs.pop().map(|(_, rows)| rows).unwrap_or_default()
        } else {
            tabs.into_iter()
                .map(|(title, blocks)| {
                    Block::Section(Section {
                        title: (!title.trim().is_empty()).then_some(title),
                        blocks,
                    })
                })
                .collect()
        };
        if options.page_per_section && !first {
            report.blocks.push(Block::PageBreak);
        }
        first = false;
        report.blocks.push(Block::Section(Section {
            title: (!section.title.trim().is_empty()).then(|| section.title.clone()),
            blocks,
        }));
    }
    Some(report)
}

/// The blocks for `cells` in document order, for a "selected cells"
/// export.
pub fn cells_report(input: &NotebookReportInput<'_>, cells: &[String]) -> ReportDoc {
    let mut report = ReportDoc {
        meta: report_meta(input),
        blocks: Vec::new(),
    };
    for cell in &input.doc.cells {
        if cells.contains(&cell.id) {
            report.blocks.extend(cell_blocks(input, &cell.id));
        }
    }
    report
}
