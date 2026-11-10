//! A styled, multi-sheet `.xlsx` writer — the export that keeps what the
//! grid shows: column order, widths, hidden and frozen columns, number and
//! date formats, conditional formatting, and computed columns as live Excel
//! formulas (with their computed values cached, so the file reads correctly
//! even before Excel recalculates).
//!
//! Pure: a [`Workbook`] in, bytes out. The results view builds the workbook
//! from its grid (`results/excel_export.rs`); the plain single-sheet writer
//! in `result_export_xlsx` still serves exports with no grid behind them.

use sqlly_datatable::{ConditionalCondition, ConditionalKind, RgbaColor};

use crate::result_export::{excel_column_label, xml_escape};
use crate::result_export_xlsx::{sanitize_excel_name, zip_archive};

const XML_DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Workbook {
    pub sheets: Vec<Sheet>,
    /// Workbook-level names (`@variables`), as `(name, constant formula)`.
    pub names: Vec<(String, String)>,
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
    pub rules: Vec<ConditionalKind>,
}

/// One cell format: number format, indent, bold.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Xf {
    num_fmt: Option<String>,
    indent: u8,
    bold: bool,
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
        Cell::Bool(b) => format!("<c r=\"{r}\"{s} t=\"b\"><v>{}</v></c>", u8::from(*b)),
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
            });
            cell_xml(r, cell, style, styles, col_fmt)
        }
    }
}

fn condition_rule(
    kind: &ConditionalKind,
    first_cell: &str,
    priority: usize,
    styles: &mut Styles,
) -> String {
    match kind {
        ConditionalKind::Rule { condition, style } => {
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
        ConditionalKind::ColorScale { min, max } => format!(
            "<cfRule type=\"colorScale\" priority=\"{priority}\"><colorScale><cfvo type=\"min\"/><cfvo type=\"max\"/><color rgb=\"{}\"/><color rgb=\"{}\"/></colorScale></cfRule>",
            argb(*min),
            argb(*max)
        ),
        ConditionalKind::DataBar { color } => format!(
            "<cfRule type=\"dataBar\" priority=\"{priority}\"><dataBar><cfvo type=\"min\"/><cfvo type=\"max\"/><color rgb=\"{}\"/></dataBar></cfRule>",
            argb(*color)
        ),
    }
}

fn worksheet_xml(sheet: &Sheet, styles: &mut Styles, table_rel: bool) -> String {
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
            x.push_str(&cell_xml(&r, cell, style, styles, &fmt));
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
    if table_rel {
        x.push_str("<tableParts count=\"1\"><tablePart r:id=\"rId1\"/></tableParts>");
    }
    x.push_str("</worksheet>");
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
    for (i, sheet) in sheets.iter().enumerate() {
        let n = i + 1;
        let has_table = table_ok(sheet);
        parts.push((
            format!("xl/worksheets/sheet{n}.xml"),
            worksheet_xml(sheet, &mut styles, has_table).into_bytes(),
        ));
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
            parts.push((
                format!("xl/worksheets/_rels/sheet{n}.xml.rels"),
                format!(
                    "{XML_DECLARATION}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL_NS}/table\" Target=\"../tables/table{table_id}.xml\"/></Relationships>"
                )
                .into_bytes(),
            ));
            overrides.push_str(&format!(
                "<Override PartName=\"/xl/tables/table{table_id}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml\"/>"
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
    let mut out: Vec<(String, Vec<u8>)> = vec![
        (
            "[Content_Types].xml".into(),
            format!(
                "{XML_DECLARATION}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>{overrides}</Types>"
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

/// A strftime pattern (`-- sqlly format: date=`) as an Excel format code.
pub fn strftime_to_excel(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            if c.is_ascii_alphabetic() {
                out.push('\\');
            }
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str("yyyy"),
            Some('y') => out.push_str("yy"),
            Some('m') => out.push_str("mm"),
            Some('b') | Some('h') => out.push_str("mmm"),
            Some('B') => out.push_str("mmmm"),
            Some('d') => out.push_str("dd"),
            Some('e') => out.push('d'),
            Some('a') => out.push_str("ddd"),
            Some('A') => out.push_str("dddd"),
            Some('H') | Some('I') => out.push_str("hh"),
            Some('M') => out.push_str("mm"),
            Some('S') => out.push_str("ss"),
            Some('p') => out.push_str("AM/PM"),
            Some('F') => out.push_str("yyyy-mm-dd"),
            Some('T') => out.push_str("hh:mm:ss"),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
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
                            ConditionalKind::Rule {
                                condition: ConditionalCondition::GreaterThan(1.0),
                                style: sqlly_datatable::ConditionalStyle {
                                    foreground: Some(RgbaColor::new(1.0, 0.0, 0.0, 1.0)),
                                    background: None,
                                    bold: true,
                                },
                            },
                            ConditionalKind::DataBar {
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
