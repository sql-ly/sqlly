//! A styled, multi-sheet `.xlsx` writer — the export that keeps what the
//! grid shows: column order, widths, hidden and frozen columns, number and
//! date formats, conditional formatting, and computed columns as live Excel
//! formulas (with their computed values cached, so the file reads correctly
//! even before Excel recalculates).
//!
//! Pure: a [`Workbook`] in, bytes out. The app's results view builds the
//! workbook from its grid, the notebook report exporter from a
//! [`crate::report::ReportDoc`]; the plain single-sheet writer in
//! [`super::plain`] serves exports with no grid behind them.
//!
//! Text cells that a spreadsheet would read as a formula once edited
//! (`=`, `+`, `-`, `@`, per [`sqlly_core::formula_guard`]) are written with
//! Excel's quote prefix, so the value stays exactly as stored and stays text.

use sqlly_core::formula_guard::needs_formula_guard;

use super::zip::zip_archive;
use super::{excel_column_label, sanitize_excel_name};
use crate::report::text::xml_escape;

pub use crate::report::format::strftime_to_excel;

const XML_DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Workbook {
    pub sheets: Vec<Sheet>,
    /// Workbook-level names (`@variables`), as `(name, constant formula)`.
    pub names: Vec<(String, String)>,
    /// Pictures floating over sheets (report charts).
    pub images: Vec<SheetImage>,
}

/// A PNG anchored at a cell of a sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetImage {
    /// Index into [`Workbook::sheets`].
    pub sheet: usize,
    /// Zero-based top-left anchor cell.
    pub column: usize,
    pub row: usize,
    /// Display size in pixels.
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sheet {
    pub name: String,
    pub columns: Vec<SheetColumn>,
    /// Header rows above the data (bold). Usually one.
    pub header_rows: Vec<Vec<String>>,
    pub rows: Vec<Vec<Cell>>,
    /// Leading columns pinned while scrolling sideways.
    pub freeze_columns: usize,
    /// Register the data as an Excel table (banded rows, filter buttons).
    /// Needs exactly one header row of distinct names.
    pub table: bool,
    pub conditional: Vec<ConditionalBlock>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SheetColumn {
    /// Width in Excel character units.
    pub width: Option<f64>,
    pub hidden: bool,
    /// Excel number format code for the column's cells.
    pub number_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Empty,
    Number(f64),
    /// A number written with exactly these digits (a plain decimal
    /// literal, already checked), so `decimal(38,10)` text keeps every digit
    /// the file can hold.
    NumberLiteral(String),
    Text(String),
    Bool(bool),
    /// An Excel error value (`#DIV/0!`).
    Error(String),
    /// A live formula (without the leading `=`) and its computed value.
    Formula {
        formula: String,
        cached: Box<Cell>,
    },
    /// A cell indented `indent` levels and optionally bold (pivot labels
    /// and totals).
    Styled {
        cell: Box<Cell>,
        indent: u8,
        bold: bool,
    },
}

/// Conditional formatting for one column's data cells, in priority order.
#[derive(Debug, Clone, PartialEq)]
pub struct ConditionalBlock {
    pub column: usize,
    pub rules: Vec<ConditionalRule>,
}

/// An sRGB color, channels `0.0..=1.0` (alpha is ignored by Excel).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RgbaColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl RgbaColor {
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConditionalCondition {
    IsNull,
    IsNotNull,
    GreaterThan(f64),
    LessThan(f64),
    /// Inclusive, bounds in either order.
    Between(f64, f64),
    EqualsText(String),
    ContainsText(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ConditionalStyle {
    pub foreground: Option<RgbaColor>,
    pub background: Option<RgbaColor>,
    pub bold: bool,
}

/// One conditional-formatting rule, mirroring the grid's rule kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum ConditionalRule {
    Rule {
        condition: ConditionalCondition,
        style: ConditionalStyle,
    },
    ColorScale {
        min: RgbaColor,
        max: RgbaColor,
    },
    DataBar {
        color: RgbaColor,
    },
}

/// One cell format: number format, indent, bold, quote prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Xf {
    num_fmt: Option<String>,
    indent: u8,
    bold: bool,
    quote: bool,
}

#[derive(Default)]
struct Styles {
    xfs: Vec<Xf>,
    num_fmts: Vec<String>,
    dxfs: Vec<String>,
}

impl Styles {
    fn new() -> Self {
        Self {
            xfs: vec![Xf {
                num_fmt: None,
                indent: 0,
                bold: false,
                quote: false,
            }],
            ..Default::default()
        }
    }

    fn xf(&mut self, xf: Xf) -> usize {
        if let Some(fmt) = &xf.num_fmt
            && !self.num_fmts.contains(fmt)
        {
            self.num_fmts.push(fmt.clone());
        }
        match self.xfs.iter().position(|x| *x == xf) {
            Some(i) => i,
            None => {
                self.xfs.push(xf);
                self.xfs.len() - 1
            }
        }
    }

    /// The style `style` with Excel's quote prefix turned on.
    fn quoted(&mut self, style: usize) -> usize {
        let mut xf = self.xfs.get(style).cloned().unwrap_or(Xf {
            num_fmt: None,
            indent: 0,
            bold: false,
            quote: false,
        });
        xf.quote = true;
        self.xf(xf)
    }

    fn dxf(&mut self, fg: Option<RgbaColor>, bg: Option<RgbaColor>, bold: bool) -> usize {
        let mut xml = String::from("<dxf>");
        if fg.is_some() || bold {
            xml.push_str("<font>");
            if bold {
                xml.push_str("<b/>");
            }
            if let Some(c) = fg {
                xml.push_str(&format!("<color rgb=\"{}\"/>", argb(c)));
            }
            xml.push_str("</font>");
        }
        if let Some(c) = bg {
            xml.push_str(&format!(
                "<fill><patternFill patternType=\"solid\"><fgColor rgb=\"{0}\"/><bgColor rgb=\"{0}\"/></patternFill></fill>",
                argb(c)
            ));
        }
        xml.push_str("</dxf>");
        self.dxfs.push(xml);
        self.dxfs.len() - 1
    }

    fn num_fmt_id(&self, code: &Option<String>) -> usize {
        match code {
            None => 0,
            Some(c) => 164 + self.num_fmts.iter().position(|f| f == c).unwrap_or(0),
        }
    }

    fn xml(&self) -> String {
        let mut x = format!("{XML_DECLARATION}<styleSheet xmlns=\"{MAIN_NS}\">");
        if !self.num_fmts.is_empty() {
            x.push_str(&format!("<numFmts count=\"{}\">", self.num_fmts.len()));
            for (i, f) in self.num_fmts.iter().enumerate() {
                x.push_str(&format!(
                    "<numFmt numFmtId=\"{}\" formatCode=\"{}\"/>",
                    164 + i,
                    xml_escape(f)
                ));
            }
            x.push_str("</numFmts>");
        }
        x.push_str("<fonts count=\"2\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font><font><b/><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>");
        x.push_str("<fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills>");
        x.push_str("<borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>");
        x.push_str("<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>");
        x.push_str(&format!("<cellXfs count=\"{}\">", self.xfs.len()));
        for xf in &self.xfs {
            let id = self.num_fmt_id(&xf.num_fmt);
            x.push_str(&format!(
                "<xf numFmtId=\"{id}\" fontId=\"{}\" fillId=\"0\" borderId=\"0\" xfId=\"0\"{}{}",
                usize::from(xf.bold),
                if id != 0 {
                    " applyNumberFormat=\"1\""
                } else {
                    ""
                },
                if xf.bold { " applyFont=\"1\"" } else { "" },
            ));
            if xf.quote {
                x.push_str(" quotePrefix=\"1\"");
            }
            if xf.indent > 0 {
                x.push_str(&format!(
                    " applyAlignment=\"1\"><alignment indent=\"{}\"/></xf>",
                    xf.indent
                ));
            } else {
                x.push_str("/>");
            }
        }
        x.push_str("</cellXfs>");
        x.push_str("<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>");
        x.push_str(&format!("<dxfs count=\"{}\">", self.dxfs.len()));
        for d in &self.dxfs {
            x.push_str(d);
        }
        x.push_str("</dxfs></styleSheet>");
        x
    }
}

fn argb(c: RgbaColor) -> String {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("FF{:02X}{:02X}{:02X}", b(c.r), b(c.g), b(c.b))
}

/// A sheet name Excel accepts: at most 31 characters, none of `[]:*?/\`,
/// and unique in the workbook.
fn sheet_name(raw: &str, taken: &[String]) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if "[]:*?/\\".contains(c) { '_' } else { c })
        .collect();
    let base: String = cleaned.trim().trim_matches('\'').chars().take(31).collect();
    let base = if base.is_empty() {
        "Sheet".to_string()
    } else {
        base
    };
    let mut name = base.clone();
    let mut n = 2;
    while taken.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
        let suffix = format!(" ({n})");
        name = format!(
            "{}{suffix}",
            base.chars().take(31 - suffix.len()).collect::<String>()
        );
        n += 1;
    }
    name
}

fn number_text(n: f64) -> String {
    // Shortest round-trip form; Excel reads it exactly.
    format!("{n}")
}

fn cell_xml(
    r: &str,
    cell: &Cell,
    style: usize,
    styles: &mut Styles,
    col_fmt: &Option<String>,
    quoted: bool,
) -> String {
    let s = if style == 0 {
        String::new()
    } else {
        format!(" s=\"{style}\"")
    };
    match cell {
        Cell::Empty if style == 0 => String::new(),
        Cell::Empty => format!("<c r=\"{r}\"{s}/>"),
        Cell::Number(n) if n.is_finite() => {
            format!("<c r=\"{r}\"{s}><v>{}</v></c>", number_text(*n))
        }
        Cell::Number(_) => String::new(),
        Cell::NumberLiteral(n) => format!("<c r=\"{r}\"{s}><v>{}</v></c>", xml_escape(n)),
        Cell::Bool(b) => format!("<c r=\"{r}\"{s} t=\"b\"><v>{}</v></c>", u8::from(*b)),
        Cell::Text(t) if needs_formula_guard(t) && !quoted => {
            let style = styles.quoted(style);
            cell_xml(r, cell, style, styles, col_fmt, true)
        }
        Cell::Text(t) => format!(
            "<c r=\"{r}\"{s} t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",
            xml_escape(t)
        ),
        Cell::Error(e) => format!("<c r=\"{r}\"{s} t=\"e\"><v>{}</v></c>", xml_escape(e)),
        Cell::Formula { formula, cached } => {
            let f = format!("<f>{}</f>", xml_escape(formula));
            match cached.as_ref() {
                Cell::Number(n) if n.is_finite() => {
                    format!("<c r=\"{r}\"{s}>{f}<v>{}</v></c>", number_text(*n))
                }
                Cell::Bool(b) => format!("<c r=\"{r}\"{s} t=\"b\">{f}<v>{}</v></c>", u8::from(*b)),
                Cell::Text(t) => {
                    format!("<c r=\"{r}\"{s} t=\"str\">{f}<v>{}</v></c>", xml_escape(t))
                }
                Cell::Error(e) => {
                    format!("<c r=\"{r}\"{s} t=\"e\">{f}<v>{}</v></c>", xml_escape(e))
                }
                _ => format!("<c r=\"{r}\"{s}>{f}</c>"),
            }
        }
        Cell::Styled { cell, indent, bold } => {
            let style = styles.xf(Xf {
                num_fmt: col_fmt.clone(),
                indent: *indent,
                bold: *bold,
                quote: false,
            });
            cell_xml(r, cell, style, styles, col_fmt, false)
        }
    }
}

fn condition_rule(
    kind: &ConditionalRule,
    first_cell: &str,
    priority: usize,
    styles: &mut Styles,
) -> String {
    match kind {
        ConditionalRule::Rule { condition, style } => {
            let dxf = styles.dxf(style.foreground, style.background, style.bold);
            let head = |ty: &str, extra: &str| {
                format!(
                    "<cfRule type=\"{ty}\" dxfId=\"{dxf}\" priority=\"{priority}\" stopIfTrue=\"1\"{extra}>"
                )
            };
            match condition {
                ConditionalCondition::IsNull => format!(
                    "{}<formula>LEN(TRIM({first_cell}))=0</formula></cfRule>",
                    head("containsBlanks", "")
                ),
                ConditionalCondition::IsNotNull => format!(
                    "{}<formula>LEN(TRIM({first_cell}))&gt;0</formula></cfRule>",
                    head("notContainsBlanks", "")
                ),
                ConditionalCondition::GreaterThan(n) => format!(
                    "{}<formula>{}</formula></cfRule>",
                    head("cellIs", " operator=\"greaterThan\""),
                    number_text(*n)
                ),
                ConditionalCondition::LessThan(n) => format!(
                    "{}<formula>{}</formula></cfRule>",
                    head("cellIs", " operator=\"lessThan\""),
                    number_text(*n)
                ),
                ConditionalCondition::Between(a, b) => format!(
                    "{}<formula>{}</formula><formula>{}</formula></cfRule>",
                    head("cellIs", " operator=\"between\""),
                    number_text(a.min(*b)),
                    number_text(a.max(*b))
                ),
                ConditionalCondition::EqualsText(t) => format!(
                    "{}<formula>{}</formula></cfRule>",
                    head("cellIs", " operator=\"equal\""),
                    xml_escape(&format!("\"{}\"", t.replace('"', "\"\"")))
                ),
                ConditionalCondition::ContainsText(t) => {
                    let quoted = format!("\"{}\"", t.replace('"', "\"\""));
                    format!(
                        "{}<formula>{}</formula></cfRule>",
                        head(
                            "containsText",
                            &format!(" operator=\"containsText\" text=\"{}\"", xml_escape(t))
                        ),
                        xml_escape(&format!("NOT(ISERROR(SEARCH({quoted},{first_cell})))"))
                    )
                }
            }
        }
        ConditionalRule::ColorScale { min, max } => format!(
            "<cfRule type=\"colorScale\" priority=\"{priority}\"><colorScale><cfvo type=\"min\"/><cfvo type=\"max\"/><color rgb=\"{}\"/><color rgb=\"{}\"/></colorScale></cfRule>",
            argb(*min),
            argb(*max)
        ),
        ConditionalRule::DataBar { color } => format!(
            "<cfRule type=\"dataBar\" priority=\"{priority}\"><dataBar><cfvo type=\"min\"/><cfvo type=\"max\"/><color rgb=\"{}\"/></dataBar></cfRule>",
            argb(*color)
        ),
    }
}

fn worksheet_xml(sheet: &Sheet, styles: &mut Styles, table_rel: bool, drawing_rel: bool) -> String {
    let ncols = sheet
        .columns
        .len()
        .max(sheet.header_rows.iter().map(Vec::len).max().unwrap_or(0))
        .max(1);
    let header_count = sheet.header_rows.len();
    let last_row = header_count + sheet.rows.len();
    let last_col = excel_column_label(ncols);
    let mut x = format!("{XML_DECLARATION}<worksheet xmlns=\"{MAIN_NS}\" xmlns:r=\"{REL_NS}\">");
    x.push_str(&format!(
        "<dimension ref=\"A1:{last_col}{}\"/>",
        last_row.max(1)
    ));
    // Frozen header row(s) and pinned columns.
    let ysplit = header_count;
    let xsplit = sheet.freeze_columns.min(ncols);
    if xsplit > 0 || ysplit > 0 {
        let top_left = format!("{}{}", excel_column_label(xsplit + 1), ysplit + 1);
        let pane = match (xsplit > 0, ysplit > 0) {
            (true, true) => "bottomRight",
            (true, false) => "topRight",
            _ => "bottomLeft",
        };
        x.push_str("<sheetViews><sheetView workbookViewId=\"0\"><pane");
        if xsplit > 0 {
            x.push_str(&format!(" xSplit=\"{xsplit}\""));
        }
        if ysplit > 0 {
            x.push_str(&format!(" ySplit=\"{ysplit}\""));
        }
        x.push_str(&format!(
            " topLeftCell=\"{top_left}\" activePane=\"{pane}\" state=\"frozen\"/></sheetView></sheetViews>"
        ));
    }
    if sheet.columns.iter().any(|c| c.width.is_some() || c.hidden) {
        x.push_str("<cols>");
        for (i, c) in sheet.columns.iter().enumerate() {
            if c.width.is_none() && !c.hidden {
                continue;
            }
            x.push_str(&format!("<col min=\"{0}\" max=\"{0}\"", i + 1));
            match c.width {
                Some(w) => x.push_str(&format!(
                    " width=\"{:.2}\" customWidth=\"1\"",
                    w.clamp(1.0, 255.0)
                )),
                None => x.push_str(" width=\"12\""),
            }
            if c.hidden {
                x.push_str(" hidden=\"1\"");
            }
            x.push_str("/>");
        }
        x.push_str("</cols>");
    }
    x.push_str("<sheetData>");
    let header_style = styles.xf(Xf {
        num_fmt: None,
        indent: 0,
        bold: true,
        quote: false,
    });
    for (h, header) in sheet.header_rows.iter().enumerate() {
        x.push_str(&format!("<row r=\"{}\">", h + 1));
        for (i, text) in header.iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            let r = format!("{}{}", excel_column_label(i + 1), h + 1);
            x.push_str(&cell_xml(
                &r,
                &Cell::Text(text.clone()),
                header_style,
                styles,
                &None,
                false,
            ));
        }
        x.push_str("</row>");
    }
    let col_styles: Vec<usize> = sheet
        .columns
        .iter()
        .map(|c| {
            if c.number_format.is_some() {
                styles.xf(Xf {
                    num_fmt: c.number_format.clone(),
                    indent: 0,
                    bold: false,
                    quote: false,
                })
            } else {
                0
            }
        })
        .collect();
    for (ri, row) in sheet.rows.iter().enumerate() {
        let n = header_count + ri + 1;
        x.push_str(&format!("<row r=\"{n}\">"));
        for (ci, cell) in row.iter().enumerate() {
            let r = format!("{}{n}", excel_column_label(ci + 1));
            let style = col_styles.get(ci).copied().unwrap_or(0);
            let fmt = sheet.columns.get(ci).and_then(|c| c.number_format.clone());
            x.push_str(&cell_xml(&r, cell, style, styles, &fmt, false));
        }
        x.push_str("</row>");
    }
    x.push_str("</sheetData>");
    if !sheet.rows.is_empty() {
        let mut priority = 1;
        for block in &sheet.conditional {
            if block.rules.is_empty() {
                continue;
            }
            let letter = excel_column_label(block.column + 1);
            let first = header_count + 1;
            let range = format!("{letter}{first}:{letter}{last_row}");
            x.push_str(&format!("<conditionalFormatting sqref=\"{range}\">"));
            for rule in &block.rules {
                x.push_str(&condition_rule(
                    rule,
                    &format!("{letter}{first}"),
                    priority,
                    styles,
                ));
                priority += 1;
            }
            x.push_str("</conditionalFormatting>");
        }
    }
    if drawing_rel {
        x.push_str("<drawing r:id=\"rId2\"/>");
    }
    if table_rel {
        x.push_str("<tableParts count=\"1\"><tablePart r:id=\"rId1\"/></tableParts>");
    }
    x.push_str("</worksheet>");
    x
}

const EMU_PER_PX: u64 = 9525;

fn drawing_xml(images: &[(usize, &SheetImage)]) -> String {
    let mut x = format!(
        "{XML_DECLARATION}<xdr:wsDr xmlns:xdr=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"{REL_NS}\">"
    );
    for (k, (_, image)) in images.iter().enumerate() {
        let (cx, cy) = (
            u64::from(image.width) * EMU_PER_PX,
            u64::from(image.height) * EMU_PER_PX,
        );
        x.push_str(&format!(
            "<xdr:oneCellAnchor><xdr:from><xdr:col>{}</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:ext cx=\"{cx}\" cy=\"{cy}\"/><xdr:pic><xdr:nvPicPr><xdr:cNvPr id=\"{}\" name=\"{}\" descr=\"{}\"/><xdr:cNvPicPr><a:picLocks noChangeAspect=\"1\"/></xdr:cNvPicPr></xdr:nvPicPr><xdr:blipFill><a:blip r:embed=\"rId{}\"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill><xdr:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic><xdr:clientData/></xdr:oneCellAnchor>",
            image.column,
            image.row,
            k + 2,
            xml_escape(&image.name),
            xml_escape(&image.name),
            k + 1
        ));
    }
    x.push_str("</xdr:wsDr>");
    x
}

fn table_xml(id: usize, sheet: &Sheet) -> String {
    let names = &sheet.header_rows[0];
    let last = format!(
        "{}{}",
        excel_column_label(names.len().max(1)),
        sheet.rows.len() + 1
    );
    let reference = format!("A1:{last}");
    let display = sanitize_excel_name(&format!("{}_{id}", sheet.name));
    let mut x = format!(
        "{XML_DECLARATION}<table xmlns=\"{MAIN_NS}\" id=\"{id}\" name=\"{display}\" displayName=\"{display}\" ref=\"{reference}\" totalsRowShown=\"0\"><autoFilter ref=\"{reference}\"/><tableColumns count=\"{}\">",
        names.len()
    );
    for (i, name) in names.iter().enumerate() {
        x.push_str(&format!(
            "<tableColumn id=\"{}\" name=\"{}\"/>",
            i + 1,
            xml_escape(name)
        ));
    }
    x.push_str("</tableColumns><tableStyleInfo name=\"TableStyleLight9\" showFirstColumn=\"0\" showLastColumn=\"0\" showRowStripes=\"1\" showColumnStripes=\"0\"/></table>");
    x
}

/// Whether `sheet` can carry an Excel table: one header row of distinct,
/// non-empty names and at least one data row.
fn table_ok(sheet: &Sheet) -> bool {
    if !sheet.table || sheet.header_rows.len() != 1 || sheet.rows.is_empty() {
        return false;
    }
    let names = &sheet.header_rows[0];
    let mut seen = std::collections::HashSet::new();
    names
        .iter()
        .all(|n| !n.trim().is_empty() && seen.insert(n.to_lowercase()))
}

/// The workbook as `.xlsx` bytes.
pub fn write(workbook: &Workbook) -> Vec<u8> {
    let mut styles = Styles::new();
    let mut names: Vec<String> = Vec::new();
    let sheets: Vec<Sheet> = workbook
        .sheets
        .iter()
        .map(|s| {
            let name = sheet_name(&s.name, &names);
            names.push(name.clone());
            Sheet { name, ..s.clone() }
        })
        .collect();
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    let mut overrides = String::new();
    let mut workbook_rels = String::new();
    let mut sheet_entries = String::new();
    let mut table_id = 0;
    let mut drawing_id = 0;
    let mut media_id = 0;
    for (i, sheet) in sheets.iter().enumerate() {
        let n = i + 1;
        let has_table = table_ok(sheet);
        let images: Vec<(usize, &SheetImage)> = workbook
            .images
            .iter()
            .filter(|img| img.sheet == i && !img.png.is_empty())
            .enumerate()
            .collect();
        let has_drawing = !images.is_empty();
        parts.push((
            format!("xl/worksheets/sheet{n}.xml"),
            worksheet_xml(sheet, &mut styles, has_table, has_drawing).into_bytes(),
        ));
        let mut sheet_rels = String::new();
        overrides.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
        ));
        workbook_rels.push_str(&format!(
            "<Relationship Id=\"rId{n}\" Type=\"{REL_NS}/worksheet\" Target=\"worksheets/sheet{n}.xml\"/>"
        ));
        sheet_entries.push_str(&format!(
            "<sheet name=\"{}\" sheetId=\"{n}\" r:id=\"rId{n}\"/>",
            xml_escape(&sheet.name)
        ));
        if has_table {
            table_id += 1;
            parts.push((
                format!("xl/tables/table{table_id}.xml"),
                table_xml(table_id, sheet).into_bytes(),
            ));
            sheet_rels.push_str(&format!(
                "<Relationship Id=\"rId1\" Type=\"{REL_NS}/table\" Target=\"../tables/table{table_id}.xml\"/>"
            ));
            overrides.push_str(&format!(
                "<Override PartName=\"/xl/tables/table{table_id}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml\"/>"
            ));
        }
        if has_drawing {
            drawing_id += 1;
            sheet_rels.push_str(&format!(
                "<Relationship Id=\"rId2\" Type=\"{REL_NS}/drawing\" Target=\"../drawings/drawing{drawing_id}.xml\"/>"
            ));
            let mut drawing_rels = String::new();
            for (k, (_, image)) in images.iter().enumerate() {
                media_id += 1;
                parts.push((format!("xl/media/image{media_id}.png"), image.png.clone()));
                drawing_rels.push_str(&format!(
                    "<Relationship Id=\"rId{}\" Type=\"{REL_NS}/image\" Target=\"../media/image{media_id}.png\"/>",
                    k + 1
                ));
            }
            parts.push((
                format!("xl/drawings/drawing{drawing_id}.xml"),
                drawing_xml(&images).into_bytes(),
            ));
            parts.push((
                format!("xl/drawings/_rels/drawing{drawing_id}.xml.rels"),
                format!(
                    "{XML_DECLARATION}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{drawing_rels}</Relationships>"
                )
                .into_bytes(),
            ));
            overrides.push_str(&format!(
                "<Override PartName=\"/xl/drawings/drawing{drawing_id}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.drawing+xml\"/>"
            ));
        }
        if !sheet_rels.is_empty() {
            parts.push((
                format!("xl/worksheets/_rels/sheet{n}.xml.rels"),
                format!(
                    "{XML_DECLARATION}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{sheet_rels}</Relationships>"
                )
                .into_bytes(),
            ));
        }
    }
    let styles_rel = sheets.len() + 1;
    workbook_rels.push_str(&format!(
        "<Relationship Id=\"rId{styles_rel}\" Type=\"{REL_NS}/styles\" Target=\"styles.xml\"/>"
    ));
    let mut defined = String::new();
    if !workbook.names.is_empty() {
        defined.push_str("<definedNames>");
        for (name, value) in &workbook.names {
            defined.push_str(&format!(
                "<definedName name=\"{}\">{}</definedName>",
                xml_escape(name),
                xml_escape(value)
            ));
        }
        defined.push_str("</definedNames>");
    }
    let png_default = if media_id > 0 {
        "<Default Extension=\"png\" ContentType=\"image/png\"/>"
    } else {
        ""
    };
    let mut out: Vec<(String, Vec<u8>)> = vec![
        (
            "[Content_Types].xml".into(),
            format!(
                "{XML_DECLARATION}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>{png_default}<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>{overrides}</Types>"
            )
            .into_bytes(),
        ),
        (
            "_rels/.rels".into(),
            format!(
                "{XML_DECLARATION}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL_NS}/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>"
            )
            .into_bytes(),
        ),
        (
            "xl/workbook.xml".into(),
            format!(
                "{XML_DECLARATION}<workbook xmlns=\"{MAIN_NS}\" xmlns:r=\"{REL_NS}\"><sheets>{sheet_entries}</sheets>{defined}<calcPr calcId=\"191029\" fullCalcOnLoad=\"1\"/></workbook>"
            )
            .into_bytes(),
        ),
        (
            "xl/_rels/workbook.xml.rels".into(),
            format!(
                "{XML_DECLARATION}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{workbook_rels}</Relationships>"
            )
            .into_bytes(),
        ),
    ];
    out.extend(parts);
    // Styles last: every sheet has registered its formats by now.
    out.push(("xl/styles.xml".into(), styles.xml().into_bytes()));
    zip_archive(&out)
}

/// An `@variable` as an Excel defined name: letters, digits, `_` and `.`,
/// not starting with a digit and not readable as a cell reference (`AB12`,
/// `R1C1`) — those get a leading underscore.
pub fn defined_name(var: &str) -> String {
    let mut name = sanitize_excel_name(var);
    let looks_like_cell = {
        let letters: String = name
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        let rest = &name[letters.len()..];
        (!letters.is_empty()
            && letters.len() <= 3
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit()))
            || name.eq_ignore_ascii_case("r")
            || name.eq_ignore_ascii_case("c")
    };
    if looks_like_cell {
        name.insert(0, '_');
    }
    name
}

/// A grid column width (logical px) in Excel character units (Calibri 11:
/// about 7 px per character plus 5 px of padding).
pub fn excel_width(px: f32) -> f64 {
    ((f64::from(px) - 5.0) / 7.0).max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unzip_entry(bytes: &[u8], name: &str) -> Option<String> {
        // STORE archives: find the local header by name and read the data.
        let needle = name.as_bytes();
        // The name's local file header: the signature 30 bytes before it.
        let at = (30..bytes.len().saturating_sub(needle.len())).find(|&i| {
            &bytes[i..i + needle.len()] == needle && bytes[i - 30..i - 26] == [0x50, 0x4B, 3, 4]
        })?;
        let header = at - 30;
        let size = u32::from_le_bytes(bytes[header + 18..header + 22].try_into().ok()?) as usize;
        let data = at + needle.len();
        String::from_utf8(bytes[data..data + size].to_vec()).ok()
    }

    fn sample() -> Workbook {
        Workbook {
            sheets: vec![
                Sheet {
                    name: "Orders: West?".into(),
                    columns: vec![
                        SheetColumn {
                            width: Some(10.0),
                            ..Default::default()
                        },
                        SheetColumn {
                            number_format: Some("#,##0.00".into()),
                            ..Default::default()
                        },
                        SheetColumn {
                            hidden: true,
                            ..Default::default()
                        },
                    ],
                    header_rows: vec![vec!["Qty".into(), "Total".into(), "Note".into()]],
                    rows: vec![vec![
                        Cell::Number(2.0),
                        Cell::Formula {
                            formula: "A2*sqlly_rate".into(),
                            cached: Box::new(Cell::Number(0.5)),
                        },
                        Cell::Text("a<b".into()),
                    ]],
                    freeze_columns: 1,
                    table: true,
                    conditional: vec![ConditionalBlock {
                        column: 1,
                        rules: vec![
                            ConditionalRule::Rule {
                                condition: ConditionalCondition::GreaterThan(1.0),
                                style: ConditionalStyle {
                                    foreground: Some(RgbaColor::new(1.0, 0.0, 0.0, 1.0)),
                                    background: None,
                                    bold: true,
                                },
                            },
                            ConditionalRule::DataBar {
                                color: RgbaColor::new(0.0, 0.0, 1.0, 1.0),
                            },
                        ],
                    }],
                },
                Sheet {
                    name: "Orders: West? pivot".into(),
                    header_rows: vec![vec!["Region".into(), "Sum of Total".into()]],
                    rows: vec![vec![
                        Cell::Styled {
                            cell: Box::new(Cell::Text("West".into())),
                            indent: 1,
                            bold: false,
                        },
                        Cell::Number(3.0),
                    ]],
                    ..Default::default()
                },
            ],
            names: vec![("sqlly_rate".into(), "0.25".into())],
            images: Vec::new(),
        }
    }

    #[test]
    fn writes_sheets_styles_formulas_and_names() {
        let bytes = write(&sample());
        let workbook = unzip_entry(&bytes, "xl/workbook.xml").expect("workbook");
        assert!(
            workbook.contains("<sheet name=\"Orders_ West_\" sheetId=\"1\""),
            "{workbook}"
        );
        assert!(workbook.contains("name=\"Orders_ West_ pivot\""));
        assert!(workbook.contains("<definedName name=\"sqlly_rate\">0.25</definedName>"));
        assert!(workbook.contains("fullCalcOnLoad=\"1\""));
        let sheet = unzip_entry(&bytes, "xl/worksheets/sheet1.xml").expect("sheet1");
        assert!(sheet.contains("<f>A2*sqlly_rate</f><v>0.5</v>"), "{sheet}");
        assert!(sheet.contains("xSplit=\"1\" ySplit=\"1\""));
        assert!(sheet.contains("hidden=\"1\""));
        assert!(sheet.contains("a&lt;b"));
        assert!(sheet.contains("<conditionalFormatting sqref=\"B2:B2\">"));
        assert!(sheet.contains("operator=\"greaterThan\""));
        assert!(sheet.contains("type=\"dataBar\""));
        assert!(sheet.contains("<tablePart"));
        let styles = unzip_entry(&bytes, "xl/styles.xml").expect("styles");
        assert!(styles.contains("formatCode=\"#,##0.00\""));
        assert!(styles.contains("<alignment indent=\"1\"/>"));
        assert!(styles.contains("<dxf><font><b/><color rgb=\"FFFF0000\"/></font></dxf>"));
        let content = unzip_entry(&bytes, "[Content_Types].xml").expect("types");
        assert!(content.contains("/xl/worksheets/sheet2.xml"));
        assert!(content.contains("/xl/tables/table1.xml"));
        assert!(unzip_entry(&bytes, "xl/tables/table1.xml").is_some());
    }

    #[test]
    fn sheet_names_are_valid_and_unique() {
        let taken = vec!["Results".to_string()];
        assert_eq!(sheet_name("Results", &taken), "Results (2)");
        assert_eq!(sheet_name("a/b", &[]), "a_b");
        assert_eq!(sheet_name(&"x".repeat(40), &[]).chars().count(), 31);
    }

    #[test]
    fn variable_names_never_read_as_cells() {
        assert_eq!(defined_name("rate"), "rate");
        assert_eq!(defined_name("Q1"), "_Q1");
        assert_eq!(defined_name("abc123"), "_abc123");
        assert_eq!(defined_name("region2026x"), "region2026x");
        assert_eq!(defined_name("r"), "_r");
    }

    #[test]
    fn strftime_patterns_become_excel_codes() {
        assert_eq!(strftime_to_excel("%Y-%m-%d"), "yyyy-mm-dd");
        assert_eq!(strftime_to_excel("%d %b %Y %H:%M"), "dd mmm yyyy hh:mm");
        assert_eq!(strftime_to_excel("%I:%M %p"), "hh:mm AM/PM");
    }
}
