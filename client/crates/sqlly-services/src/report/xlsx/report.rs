//! The notebook report as a workbook: a summary sheet (title, metadata,
//! KPIs, checks, notes and a sheet index) followed by one sheet per table
//! and per chart's data. Numbers stay numbers (exact digits where the file
//! can hold them), dates become Excel dates with a matching format, and
//! text that would read as a formula is quote-prefixed.

use sqlly_core::CellValue;

use super::workbook::{Cell, Sheet, SheetColumn, SheetImage, Workbook};
use crate::report::doc::{Block, ReportDoc, TableBlock, walk};
use crate::report::fidelity::{FidelityKind, FidelityReport};
use crate::report::format::{
    ColumnKind, Temporal, excel_format, format_cell, hex_bytes, numeric_literal,
    parse_temporal_zoned, significant_digits,
};
use crate::report::labels::ReportLabels;
use crate::report::markdown;
use crate::report::{RenderOptions, ReportFormat, plan};

/// Longest text an Excel cell holds.
const MAX_CELL_TEXT: usize = 32_767;
/// Most data rows a sheet holds (Excel's row limit minus the header).
const MAX_SHEET_ROWS: usize = 1_048_575;
const MAX_SHEET_COLUMNS: usize = 16_384;
/// Integers above this many digits would lose their last digits in Excel.
const MAX_NUMBER_DIGITS: usize = 15;

struct Builder<'a> {
    labels: &'a ReportLabels,
    resolve: fn(&sqlly_core::DisplayMessage) -> String,
    fidelity: FidelityReport,
    sheets: Vec<Sheet>,
    images: Vec<SheetImage>,
    index: Vec<(String, String, String)>,
}

pub fn render_xlsx(doc: &ReportDoc, options: &RenderOptions) -> (Vec<u8>, FidelityReport) {
    let workbook = report_workbook(doc, options);
    (super::write(&workbook.0), workbook.1)
}

/// The workbook before it is zipped, for tests and callers that add sheets.
pub fn report_workbook(doc: &ReportDoc, options: &RenderOptions) -> (Workbook, FidelityReport) {
    let mut b = Builder {
        labels: &options.labels,
        resolve: options.resolve_message,
        fidelity: FidelityReport::new(ReportFormat::Xlsx),
        sheets: Vec::new(),
        images: Vec::new(),
        index: Vec::new(),
    };
    let mut narrative = false;
    let mut count = 0usize;
    walk(&doc.blocks, &mut |block| match block {
        Block::Table(t) => {
            count += 1;
            b.data_sheet(t, t.title.clone(), count);
        }
        Block::Chart(c) => {
            count += 1;
            let title = c.spec.title.clone().or_else(|| c.data.title.clone());
            let sheet = b.data_sheet(&c.data, title, count);
            b.chart_image(c, sheet, options);
        }
        Block::Markdown(_) | Block::Callout(_) | Block::Heading(_) => narrative = true,
        Block::Columns(cols) if cols.columns.len() > 1 => {
            b.fidelity.note_once(FidelityKind::ColumnsStacked)
        }
        Block::PageBreak => b.fidelity.note_once(FidelityKind::PageBreaksIgnored),
        _ => {}
    });
    if narrative {
        b.fidelity.note_once(FidelityKind::RichTextFlattened);
    }
    b.fidelity.note_once(FidelityKind::LayoutFlattened);
    let summary = b.summary(doc);
    let mut workbook = Workbook::default();
    workbook.sheets.push(summary);
    workbook.sheets.extend(b.sheets);
    workbook.images = b
        .images
        .into_iter()
        .map(|mut img| {
            img.sheet += 1;
            img
        })
        .collect();
    (workbook, b.fidelity)
}

fn text_cell(text: &str, fidelity: &mut FidelityReport, block: Option<&str>) -> Cell {
    if text.chars().count() > MAX_CELL_TEXT {
        fidelity.note(FidelityKind::CellTextTruncated, block);
        return Cell::Text(text.chars().take(MAX_CELL_TEXT).collect());
    }
    Cell::Text(text.to_string())
}

/// One value as a typed cell, noting any precision Excel cannot keep.
pub fn xlsx_cell(
    value: &CellValue,
    kind: ColumnKind,
    column: &str,
    fidelity: &mut FidelityReport,
) -> Cell {
    match value {
        CellValue::Null => Cell::Empty,
        CellValue::Bool(b) => Cell::Bool(*b),
        CellValue::I64(n) => {
            let digits = n.unsigned_abs().to_string().len();
            if digits > MAX_NUMBER_DIGITS {
                fidelity.note(
                    FidelityKind::LargeIntegersAsText {
                        column: column.to_string(),
                    },
                    None,
                );
                Cell::Text(n.to_string())
            } else {
                Cell::Number(*n as f64)
            }
        }
        CellValue::F64(n) if n.is_finite() => Cell::Number(*n),
        CellValue::F64(n) => {
            fidelity.note(
                FidelityKind::NonFiniteAsText {
                    column: column.to_string(),
                },
                None,
            );
            Cell::Text(
                if n.is_nan() {
                    "NaN"
                } else if *n > 0.0 {
                    "Infinity"
                } else {
                    "-Infinity"
                }
                .into(),
            )
        }
        CellValue::Bytes(b) => {
            text_cell(&hex_bytes(b, MAX_CELL_TEXT / 2 - 2), fidelity, Some(column))
        }
        CellValue::String(s) => {
            if kind.is_numeric()
                && let Some(literal) = numeric_literal(s)
            {
                let digits = significant_digits(literal);
                let integer = !literal.contains(['.', 'e', 'E']);
                if digits > MAX_NUMBER_DIGITS {
                    if integer {
                        fidelity.note(
                            FidelityKind::LargeIntegersAsText {
                                column: column.to_string(),
                            },
                            None,
                        );
                        return Cell::Text(literal.to_string());
                    }
                    fidelity.note(
                        FidelityKind::PrecisionRounded {
                            column: column.to_string(),
                        },
                        None,
                    );
                }
                return Cell::NumberLiteral(literal.to_string());
            }
            if kind.is_temporal()
                && let Some((t, offset)) = parse_temporal_zoned(s)
            {
                // Excel has no zones: a value with an offset is written as
                // its UTC instant, and the export says so.
                let t = match (t, offset) {
                    (Temporal::DateTime(local), Some(offset)) => {
                        if offset != 0 {
                            fidelity.note(
                                FidelityKind::TimezoneDropped {
                                    column: column.to_string(),
                                },
                                None,
                            );
                        }
                        Temporal::DateTime(
                            local
                                .checked_sub_signed(chrono::Duration::seconds(offset))
                                .unwrap_or(local),
                        )
                    }
                    (t, _) => t,
                };
                if let Some(serial) = t.excel_serial() {
                    return Cell::Number(serial);
                }
            }
            text_cell(s, fidelity, Some(column))
        }
    }
}

fn width_of(text: &str) -> f64 {
    let chars = text.chars().count().min(200) as f64;
    (chars * 1.1 + 2.0).clamp(6.0, 60.0)
}

impl Builder<'_> {
    fn data_sheet(&mut self, t: &TableBlock, title: Option<String>, n: usize) -> usize {
        let label = title
            .clone()
            .or_else(|| t.cell_id.clone())
            .unwrap_or_else(|| format!("{n}"));
        let block_name = label.clone();
        let columns = &t.columns[..t.columns.len().min(MAX_SHEET_COLUMNS)];
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        let header = super::unique_names(&names);
        let kinds: Vec<ColumnKind> = columns.iter().map(|c| c.kind()).collect();
        let rows_held = t.rows.len().min(MAX_SHEET_ROWS);
        if rows_held < t.rows.len() {
            self.fidelity.note(
                FidelityKind::RowsCapped {
                    shown: rows_held,
                    total: t.source_rows(),
                },
                Some(&block_name),
            );
        }
        let mut widths: Vec<f64> = header.iter().map(|h| width_of(h)).collect();
        let mut rows = Vec::with_capacity(rows_held);
        for (r, row) in t.rows[..rows_held].iter().enumerate() {
            let cells: Vec<Cell> = columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let value = row.get(i).unwrap_or(&CellValue::Null);
                    if r < 200 {
                        let shown = format_cell(value, kinds[i], &c.format);
                        widths[i] = widths[i].max(width_of(&shown));
                    }
                    xlsx_cell(value, kinds[i], &c.name, &mut self.fidelity)
                })
                .collect();
            rows.push(cells);
        }
        let sheet_columns = columns
            .iter()
            .zip(&kinds)
            .zip(&widths)
            .map(|((c, k), w)| SheetColumn {
                width: Some(*w),
                hidden: false,
                number_format: excel_format(&c.format, *k),
            })
            .collect();
        let completeness = self.labels.completeness(t.completeness).unwrap_or_else(|| {
            if t.data_withheld {
                self.labels.data_withheld.clone()
            } else {
                self.labels.complete.clone()
            }
        });
        self.sheets.push(Sheet {
            name: label.clone(),
            columns: sheet_columns,
            header_rows: vec![header],
            rows,
            freeze_columns: 0,
            table: true,
            conditional: Vec::new(),
        });
        self.index
            .push((label, t.source_rows().to_string(), completeness));
        self.sheets.len() - 1
    }

    fn chart_image(
        &mut self,
        c: &crate::report::doc::ChartBlock,
        sheet: usize,
        options: &RenderOptions,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let theme = crate::report::ReportTheme::for_mode(crate::report::ThemeMode::Light);
            let rendered =
                crate::report::svg::render_chart(&c.spec, &c.data, options.chart_options(theme));
            self.fidelity.note_chart(&rendered, c.spec.title.as_deref());
            if rendered.error.is_some() {
                return;
            }
            let markup = crate::report::svg::to_svg(&rendered.scene, "chart");
            match crate::report::png::rasterize(&markup, 1.0) {
                Ok(png) => {
                    self.images.push(SheetImage {
                        sheet,
                        column: c.data.columns.len() + 1,
                        row: 0,
                        width: options.chart_width.round() as u32,
                        height: options.chart_height.round() as u32,
                        png,
                        name: c
                            .spec
                            .title
                            .clone()
                            .unwrap_or_else(|| self.labels.chart.clone()),
                    });
                    self.fidelity.note_once(FidelityKind::ChartsAsImages);
                }
                Err(_) => self.fidelity.note_once(FidelityKind::ChartsAsData),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (c, sheet, options);
            self.fidelity.note_once(FidelityKind::ChartsAsData);
        }
    }

    fn summary(&mut self, doc: &ReportDoc) -> Sheet {
        let l = self.labels;
        let bold = |text: &str| Cell::Styled {
            cell: Box::new(Cell::Text(text.to_string())),
            indent: 0,
            bold: true,
        };
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        if let Some(s) = doc.meta.subtitle.as_deref().filter(|s| !s.is_empty()) {
            rows.push(vec![Cell::Text(s.to_string())]);
        }
        for (label, value) in plan::meta_lines(doc, l) {
            rows.push(vec![bold(&label), Cell::Text(value)]);
        }
        let mut kpis = Vec::new();
        let mut checks = Vec::new();
        let mut notes = Vec::new();
        walk(&doc.blocks, &mut |b| match b {
            Block::Kpi(k) => {
                let value = match k.value {
                    Some(v) if v.is_finite() => Cell::Number(v),
                    _ => Cell::Empty,
                };
                let mut row = vec![bold(&k.label), value];
                row.push(match crate::report::svg::kpi_delta_text(k, l) {
                    Some(d) => Cell::Text(d),
                    None => Cell::Empty,
                });
                row.push(match k.resolved_status() {
                    Some(s) => Cell::Text(l.status(s).to_string()),
                    None => Cell::Empty,
                });
                if let Some(caption) = k.caption.as_deref().filter(|c| !c.is_empty()) {
                    row.push(Cell::Text(caption.to_string()));
                }
                kpis.push((row, k.format.clone()));
            }
            Block::Assertion(a) => {
                let mut row = vec![
                    bold(l.assertion(a.status)),
                    Cell::Text(a.name.clone()),
                    a.expected.clone().map(Cell::Text).unwrap_or(Cell::Empty),
                    a.actual.clone().map(Cell::Text).unwrap_or(Cell::Empty),
                ];
                if let Some(message) = a.message.as_deref().filter(|m| !m.is_empty()) {
                    row.push(Cell::Text(message.to_string()));
                }
                checks.push(row);
            }
            Block::Callout(c) => {
                let text = markdown::plain_blocks(&markdown::parse(&c.text).blocks);
                notes.push(vec![bold(l.callout(c.kind)), Cell::Text(text)]);
            }
            Block::Markdown(m) => {
                let text = markdown::plain_blocks(&markdown::parse(&m.source).blocks);
                if !text.trim().is_empty() {
                    notes.push(vec![Cell::Empty, Cell::Text(text)]);
                }
            }
            Block::Heading(h) => notes.push(vec![bold(&h.text)]),
            _ => {}
        });
        let mut kpi_format: Option<String> = None;
        if !kpis.is_empty() {
            rows.push(Vec::new());
            for (row, format) in kpis {
                if kpi_format.is_none() && !format.is_default() {
                    kpi_format = excel_format(&format, ColumnKind::Float);
                }
                rows.push(row);
            }
        }
        if !checks.is_empty() {
            rows.push(Vec::new());
            rows.extend(checks);
        }
        if !notes.is_empty() {
            rows.push(Vec::new());
            rows.extend(notes);
        }
        if !self.index.is_empty() {
            rows.push(Vec::new());
            rows.push(vec![
                bold(&l.sheet_contents),
                bold(&l.rows),
                bold(&l.status),
            ]);
            for (name, count, state) in &self.index {
                rows.push(vec![
                    Cell::Text(name.clone()),
                    Cell::Text(count.clone()),
                    Cell::Text(state.clone()),
                ]);
            }
        }
        if !self.fidelity.notes.is_empty() {
            rows.push(Vec::new());
            rows.push(vec![bold(&l.export_notes)]);
            for note in &self.fidelity.notes {
                rows.push(vec![
                    Cell::Empty,
                    Cell::Text((self.resolve)(&note.message())),
                ]);
            }
        }
        for row in &mut rows {
            for cell in row.iter_mut() {
                if let Cell::Text(t) = cell
                    && t.chars().count() > MAX_CELL_TEXT
                {
                    *t = t.chars().take(MAX_CELL_TEXT).collect();
                }
            }
        }
        Sheet {
            name: l.summary_sheet.clone(),
            columns: vec![
                SheetColumn {
                    width: Some(28.0),
                    ..Default::default()
                },
                SheetColumn {
                    width: Some(64.0),
                    number_format: kpi_format,
                    ..Default::default()
                },
                SheetColumn {
                    width: Some(28.0),
                    ..Default::default()
                },
                SheetColumn {
                    width: Some(28.0),
                    ..Default::default()
                },
            ],
            header_rows: vec![vec![doc.meta.title.clone()]],
            rows,
            freeze_columns: 0,
            table: false,
            conditional: Vec::new(),
        }
    }
}
