//! The report IR every exporter reads: notebook metadata plus an ordered
//! list of blocks. A notebook (or the CLI runner) builds one [`ReportDoc`],
//! templates rearrange it, [`super::redact`] strips what must not leave the
//! machine, and each renderer turns the same document into its format, so
//! values, titles, series and ordering agree across HTML, PDF, XLSX and the
//! rest.

use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

use super::chart::ChartSpec;
use super::format::{ColumnKind, ValueFormat};
use super::templates::TemplateKind;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReportDoc {
    pub meta: ReportMeta,
    #[serde(default)]
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReportMeta {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Who the report is from, as the user labels it (never an account id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Supplied by the caller (RFC 3339) so a render is reproducible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notebook_id: Option<String>,
    /// Content hash of the notebook version the report was run from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notebook_version: Option<String>,
    #[serde(default)]
    pub parameters: Vec<ReportParameter>,
    #[serde(default)]
    pub connections: Vec<ConnectionLabel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<TemplateKind>,
    #[serde(default)]
    pub layout: ReportLayout,
    /// Row data was withheld for a structure-only export.
    #[serde(default)]
    pub data_withheld: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReportParameter {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub sensitive: bool,
}

/// A connection the report read from, by display label only: never a host,
/// login or connection string.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ConnectionLabel {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default)]
    pub production: bool,
}

/// Where SQL and provenance blocks go in the rendered report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// Where the block sits in the notebook.
    #[default]
    Inline,
    /// Gathered into an appendix at the end.
    Appendix,
    /// Left out of the report.
    Omit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportLayout {
    /// A cover with the title and metadata before the body.
    #[serde(default = "yes")]
    pub cover: bool,
    /// A table of contents (HTML, PDF) when there are enough headings.
    #[serde(default = "yes")]
    pub contents: bool,
    #[serde(default)]
    pub sql: Placement,
    #[serde(default)]
    pub provenance: Placement,
    /// Most rows any table shows in a paged or text format. `None` shows
    /// every row the table carries; XLSX and CSV always write them all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_row_cap: Option<usize>,
}

fn yes() -> bool {
    true
}

impl Default for ReportLayout {
    fn default() -> Self {
        Self {
            cover: true,
            contents: true,
            sql: Placement::Inline,
            provenance: Placement::Inline,
            table_row_cap: None,
        }
    }
}

/// Tables and charts dominate the size; blocks are few per report, so
/// they stay inline rather than boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Heading(Heading),
    Markdown(MarkdownBlock),
    Table(TableBlock),
    Chart(ChartBlock),
    Kpi(KpiBlock),
    Callout(Callout),
    Assertion(AssertionResult),
    Code(CodeBlock),
    Provenance(Provenance),
    PageBreak,
    Section(Section),
    Columns(Columns),
    Image(ImageBlock),
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Heading {
    /// 1 to 6; anything else is clamped.
    pub level: u8,
    pub text: String,
}

impl Heading {
    pub fn level(&self) -> u8 {
        self.level.clamp(1, 6)
    }
}

/// Markdown source from a text cell. Parsed to a safe tree at render time;
/// raw HTML in it is never passed through.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MarkdownBlock {
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Completeness {
    #[default]
    Complete,
    /// The output row cap stopped the result at `cap` rows.
    Truncated { cap: u64 },
    /// `total` rows were produced; the rest live in a spill file.
    Spilled { total: u64 },
    /// A sample of `n` rows.
    Sampled { n: u64 },
    /// The run was canceled or failed part way.
    Partial,
    /// Every row came back, but the query used a value computed from
    /// partial data.
    DerivedFromPartial,
}

impl From<crate::notebook::output::Completeness> for Completeness {
    fn from(c: crate::notebook::output::Completeness) -> Self {
        use crate::notebook::output::Completeness as N;
        match c {
            N::Complete => Completeness::Complete,
            N::Truncated { cap } => Completeness::Truncated { cap },
            N::Spilled { total } => Completeness::Spilled { total },
            N::Sampled { n } => Completeness::Sampled { n },
            N::Partial => Completeness::Partial,
            N::DerivedFromPartial => Completeness::DerivedFromPartial,
        }
    }
}

impl Completeness {
    pub fn is_complete(self) -> bool {
        self == Completeness::Complete
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TableColumn {
    pub name: String,
    /// The engine's type spelling (`decimal(18,2)`, `timestamptz`).
    #[serde(default)]
    pub data_type: String,
    /// Overrides the kind inferred from `data_type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ColumnKind>,
    #[serde(default)]
    pub format: ValueFormat,
    #[serde(default)]
    pub sensitive: bool,
}

impl TableColumn {
    pub fn new(name: impl Into<String>, data_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            data_type: data_type.into(),
            ..Default::default()
        }
    }

    pub fn with_format(mut self, format: ValueFormat) -> Self {
        self.format = format;
        self
    }

    pub fn kind(&self) -> ColumnKind {
        self.kind
            .unwrap_or_else(|| ColumnKind::infer(&self.data_type))
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TableBlock {
    /// The notebook cell the rows came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    pub columns: Vec<TableColumn>,
    #[serde(default, with = "cell_rows")]
    pub rows: Vec<Vec<CellValue>>,
    #[serde(default)]
    pub completeness: Completeness,
    /// Rows the source produced, when more than `rows` holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_rows: Option<u64>,
    /// Most rows this table shows in paged and text formats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<usize>,
    /// Rows removed by a structure-only export.
    #[serde(default)]
    pub data_withheld: bool,
}

impl TableBlock {
    pub fn new(columns: Vec<TableColumn>, rows: Vec<Vec<CellValue>>) -> Self {
        Self {
            columns,
            rows,
            ..Default::default()
        }
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name == name)
            .or_else(|| {
                self.columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(name))
            })
    }

    /// Rows the source produced, whatever this block holds.
    pub fn source_rows(&self) -> u64 {
        let held = self.rows.len() as u64;
        match self.completeness {
            Completeness::Spilled { total } => total.max(held),
            _ => self.total_rows.unwrap_or(held).max(held),
        }
    }

    /// How many rows a capped format shows, given the report-wide cap.
    pub fn shown_rows(&self, layout_cap: Option<usize>) -> usize {
        let cap = match (self.max_rows, layout_cap) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => usize::MAX,
        };
        self.rows.len().min(cap)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ChartBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    pub spec: ChartSpec,
    /// The chart's input rows, typed.
    pub data: TableBlock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Good,
    Warning,
    Critical,
    Neutral,
}

/// At or above `value`, the KPI takes `status`. The highest threshold the
/// value reaches wins, so a lower-is-better metric lists its bad levels
/// last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Threshold {
    pub value: f64,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KpiComparison {
    /// What the value is compared with ("last week", "target").
    pub label: String,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KpiBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    pub label: String,
    /// `None` when the value is unknown or withheld.
    pub value: Option<f64>,
    #[serde(default)]
    pub format: ValueFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<KpiComparison>,
    /// Overrides the status the thresholds would give.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default)]
    pub thresholds: Vec<Threshold>,
    #[serde(default)]
    pub sparkline: Vec<f64>,
    /// A rise is good news (revenue) rather than bad (error rate).
    #[serde(default = "yes")]
    pub higher_is_better: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

impl Default for KpiBlock {
    fn default() -> Self {
        Self {
            cell_id: None,
            label: String::new(),
            value: None,
            format: ValueFormat::default(),
            comparison: None,
            status: None,
            thresholds: Vec::new(),
            sparkline: Vec::new(),
            higher_is_better: true,
            caption: None,
        }
    }
}

impl KpiBlock {
    /// `value - comparison`.
    pub fn delta(&self) -> Option<f64> {
        Some(self.value? - self.comparison.as_ref()?.value)
    }

    /// The change relative to the comparison, as a fraction (`0.12` = 12%).
    pub fn delta_ratio(&self) -> Option<f64> {
        let base = self.comparison.as_ref()?.value;
        (base != 0.0).then(|| self.delta().map(|d| d / base.abs()))?
    }

    pub fn resolved_status(&self) -> Option<Status> {
        if self.status.is_some() {
            return self.status;
        }
        let value = self.value?;
        let mut sorted: Vec<&Threshold> = self.thresholds.iter().collect();
        sorted.sort_by(|a, b| a.value.total_cmp(&b.value));
        sorted
            .iter()
            .rev()
            .find(|t| value >= t.value)
            .map(|t| t.status)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalloutKind {
    /// Read straight off a result.
    Fact,
    /// Computed from results.
    Derived,
    /// A possible explanation not yet proven.
    Hypothesis,
    Recommendation,
    Warning,
    #[default]
    Note,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Callout {
    pub kind: CalloutKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Markdown.
    pub text: String,
    /// Run ids or cell ids the statement rests on.
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionStatus {
    Passed,
    Failed,
    Warning,
    Error,
    Skipped,
    /// The check could not decide (its input was incomplete, say); the
    /// message says why.
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    pub name: String,
    pub status: AssertionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CodeBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    /// `sql`, `json`, …; only used as a label and a fence info string.
    pub language: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

impl CodeBlock {
    pub fn sql(source: impl Into<String>) -> Self {
        Self {
            language: "sql".into(),
            source: source.into(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Provenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Connection display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Section {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub blocks: Vec<Block>,
}

/// A dashboard row on the 12-column grid.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Columns {
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Column {
    /// 1 to 12 grid columns.
    pub span: u8,
    pub blocks: Vec<Block>,
}

impl Column {
    pub fn span(&self) -> u8 {
        self.span.clamp(1, 12)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum ImageData {
    Svg {
        markup: String,
    },
    Png {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageBlock {
    pub alt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    pub image: ImageData,
}

/// Rows as `CellValue`'s own JSON, except that NaN and infinities (which
/// JSON numbers cannot hold) are written as `{"F64Text": "NaN"}` so a
/// document always reads back.
mod cell_rows {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use sqlly_core::CellValue;

    #[derive(Serialize, Deserialize)]
    enum Wire {
        Null,
        Bool(bool),
        I64(i64),
        F64(f64),
        F64Text(String),
        String(String),
        Bytes(Vec<u8>),
    }

    impl From<&CellValue> for Wire {
        fn from(v: &CellValue) -> Self {
            match v {
                CellValue::Null => Wire::Null,
                CellValue::Bool(b) => Wire::Bool(*b),
                CellValue::I64(n) => Wire::I64(*n),
                CellValue::F64(n) if n.is_finite() => Wire::F64(*n),
                CellValue::F64(n) => Wire::F64Text(n.to_string()),
                CellValue::String(s) => Wire::String(s.clone()),
                CellValue::Bytes(b) => Wire::Bytes(b.clone()),
            }
        }
    }

    impl From<Wire> for CellValue {
        fn from(w: Wire) -> Self {
            match w {
                Wire::Null => CellValue::Null,
                Wire::Bool(b) => CellValue::Bool(b),
                Wire::I64(n) => CellValue::I64(n),
                Wire::F64(n) => CellValue::F64(n),
                Wire::F64Text(t) => CellValue::F64(t.parse().unwrap_or(f64::NAN)),
                Wire::String(s) => CellValue::String(s),
                Wire::Bytes(b) => CellValue::Bytes(b),
            }
        }
    }

    pub fn serialize<S: Serializer>(rows: &[Vec<CellValue>], s: S) -> Result<S::Ok, S::Error> {
        let wire: Vec<Vec<Wire>> = rows
            .iter()
            .map(|r| r.iter().map(Wire::from).collect())
            .collect();
        wire.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<CellValue>>, D::Error> {
        let wire: Vec<Vec<Wire>> = Vec::deserialize(d)?;
        Ok(wire
            .into_iter()
            .map(|r| r.into_iter().map(CellValue::from).collect())
            .collect())
    }
}

mod base64_bytes {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// Every block, depth first, sections and columns included.
pub fn walk<'a>(blocks: &'a [Block], visit: &mut dyn FnMut(&'a Block)) {
    for block in blocks {
        visit(block);
        match block {
            Block::Section(section) => walk(&section.blocks, visit),
            Block::Columns(columns) => {
                for column in &columns.columns {
                    walk(&column.blocks, visit);
                }
            }
            _ => {}
        }
    }
}

/// [`walk`] with mutable access, children after their parent.
pub fn walk_mut(blocks: &mut [Block], visit: &mut dyn FnMut(&mut Block)) {
    for block in blocks.iter_mut() {
        visit(block);
        match block {
            Block::Section(section) => walk_mut(&mut section.blocks, visit),
            Block::Columns(columns) => {
                for column in &mut columns.columns {
                    walk_mut(&mut column.blocks, visit);
                }
            }
            _ => {}
        }
    }
}
