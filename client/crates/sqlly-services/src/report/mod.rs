//! Notebook reports (ADR-003, ADR-010): one [`ReportDoc`] rendered to HTML,
//! Markdown, PDF, XLSX, CSV, SVG and PNG with a [`FidelityReport`] saying
//! what each format could not carry. Pure and GPUI-free, so the app's
//! export dialog and `sqlly notebook export` produce identical files, and
//! deterministic: the same document and options give the same bytes.
//!
//! ```
//! use sqlly_services::report::{self, doc::*, ReportFormat, RenderOptions};
//! use sqlly_core::CellValue;
//!
//! let mut doc = ReportDoc::default();
//! doc.meta.title = "Weekly revenue".into();
//! doc.blocks.push(Block::Table(TableBlock::new(
//!     vec![TableColumn::new("region", "varchar"), TableColumn::new("revenue", "decimal(18,2)")],
//!     vec![vec![CellValue::String("West".into()), CellValue::String("1234.50".into())]],
//! )));
//! let out = report::render(&doc, ReportFormat::Html, &RenderOptions::default()).unwrap();
//! assert!(String::from_utf8_lossy(&out.files[0].bytes).contains("1234.50"));
//! ```

pub mod chart;
pub mod csv;
pub mod doc;
pub mod fidelity;
pub mod format;
pub mod from_notebook;
pub mod html;
pub mod labels;
pub mod markdown;
pub mod md;
#[cfg(not(target_arch = "wasm32"))]
pub mod pdf;
mod plan;
#[cfg(not(target_arch = "wasm32"))]
pub mod png;
pub mod redact;
pub mod svg;
pub mod templates;
pub mod text;
pub mod theme;
pub mod xlsx;

use serde::{Deserialize, Serialize};

pub use chart::{ChartKind, ChartSpec, ChartSpecError};
pub use doc::{Block, ReportDoc};
pub use fidelity::{FidelityKind, FidelityNote, FidelityReport};
pub use from_notebook::{NotebookReportInput, from_notebook};
pub use labels::ReportLabels;
pub use redact::{RedactionPolicy, RedactionSummary, redact};
pub use templates::{TemplateKind, apply_template};
pub use theme::{ReportTheme, ThemeMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportFormat {
    Html,
    Markdown,
    Pdf,
    Xlsx,
    Csv,
    Svg,
    Png,
}

impl ReportFormat {
    pub const ALL: [ReportFormat; 7] = [
        ReportFormat::Html,
        ReportFormat::Markdown,
        ReportFormat::Pdf,
        ReportFormat::Xlsx,
        ReportFormat::Csv,
        ReportFormat::Svg,
        ReportFormat::Png,
    ];

    pub fn extension(self) -> &'static str {
        match self {
            ReportFormat::Html => "html",
            ReportFormat::Markdown => "md",
            ReportFormat::Pdf => "pdf",
            ReportFormat::Xlsx => "xlsx",
            ReportFormat::Csv => "csv",
            ReportFormat::Svg => "svg",
            ReportFormat::Png => "png",
        }
    }

    pub fn media_type(self) -> &'static str {
        match self {
            ReportFormat::Html => "text/html",
            ReportFormat::Markdown => "text/markdown",
            ReportFormat::Pdf => "application/pdf",
            ReportFormat::Xlsx => {
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            }
            ReportFormat::Csv => "text/csv",
            ReportFormat::Svg => "image/svg+xml",
            ReportFormat::Png => "image/png",
        }
    }

    /// Whether this build can produce the format (PDF and PNG need the
    /// native font and raster stack).
    pub fn is_available(self) -> bool {
        !(cfg!(target_arch = "wasm32") && matches!(self, ReportFormat::Pdf | ReportFormat::Png))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageSize {
    #[default]
    A4,
    Letter,
}

/// How Markdown exports carry chart pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkdownImages {
    /// `assets/chart-1.svg` files next to the document.
    #[default]
    Files,
    /// `data:` URLs inside the document (one file, larger).
    Inline,
}

#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Theme for single-theme formats (PDF, SVG, PNG). HTML carries both
    /// and follows the reader's setting.
    pub theme: ThemeMode,
    pub labels: ReportLabels,
    pub page: PageSize,
    pub landscape: bool,
    /// Chart size in CSS pixels.
    pub chart_width: f32,
    pub chart_height: f32,
    /// PNG pixel density (2.0 = 192 DPI at the CSS size).
    pub png_scale: f32,
    /// Prefix CSV text cells that a spreadsheet would run as a formula.
    pub csv_formula_guard: bool,
    pub markdown_images: MarkdownImages,
    /// Shown in the HTML `lang` attribute.
    pub language: String,
    /// Turns a fidelity message into display text where a format prints
    /// the export notes (the XLSX summary). The app passes its
    /// localization formatter; the default is the English fallback.
    pub resolve_message: fn(&sqlly_core::DisplayMessage) -> String,
}

/// A message's English fallback, for callers with no localization layer.
pub fn english(message: &sqlly_core::DisplayMessage) -> String {
    message.fallback().to_string()
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Light,
            labels: ReportLabels::default(),
            page: PageSize::A4,
            landscape: false,
            chart_width: 640.0,
            chart_height: 360.0,
            png_scale: 2.0,
            csv_formula_guard: true,
            markdown_images: MarkdownImages::Files,
            language: "en".into(),
            resolve_message: english,
        }
    }
}

impl RenderOptions {
    pub fn chart_options(&self, theme: &'static ReportTheme) -> svg::ChartOptions<'_> {
        svg::ChartOptions::new(theme, &self.labels).size(self.chart_width, self.chart_height)
    }
}

/// One output file. `path` is relative ("report.html", "assets/chart-1.svg").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    pub path: String,
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderOutput {
    pub files: Vec<RenderedFile>,
    pub fidelity: FidelityReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportError {
    /// The format is not built into this target (PDF and PNG in the browser).
    Unavailable(ReportFormat),
    /// The document has nothing this format can carry (CSV without tables,
    /// SVG/PNG without charts).
    Empty(ReportFormat),
    Render(String),
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReportError::Unavailable(format) => {
                write!(f, "{} export is not available here", format.extension())
            }
            ReportError::Empty(format) => write!(
                f,
                "nothing in the report can be exported as {}",
                format.extension()
            ),
            ReportError::Render(detail) => write!(f, "report export failed: {detail}"),
        }
    }
}

impl std::error::Error for ReportError {}

impl ReportError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            ReportError::Unavailable(format) => {
                let name = format.extension().to_uppercase();
                DisplayMessage::new(
                    "notebook.report.errors.unavailable",
                    format!("{name} export isn't available in this version of SQLLY."),
                )
                .with_arg("format", name)
            }
            ReportError::Empty(format) => {
                let name = format.extension().to_uppercase();
                DisplayMessage::new(
                    "notebook.report.errors.empty",
                    format!("There's nothing in this notebook that can be exported as {name}."),
                )
                .with_arg("format", name)
            }
            ReportError::Render(detail) => DisplayMessage::new(
                "notebook.report.errors.render",
                format!("The report couldn't be exported: {detail}"),
            )
            .with_arg("detail", detail.clone()),
        }
    }
}

fn file_stem(doc: &ReportDoc) -> String {
    let slug = text::slug(&doc.meta.title);
    if slug == "section" {
        "report".into()
    } else {
        slug
    }
}

/// Renders `doc` as `format`.
pub fn render(
    doc: &ReportDoc,
    format: ReportFormat,
    options: &RenderOptions,
) -> Result<RenderOutput, ReportError> {
    let stem = file_stem(doc);
    let single = |bytes: Vec<u8>, fidelity: FidelityReport| RenderOutput {
        files: vec![RenderedFile {
            path: format!("{stem}.{}", format.extension()),
            media_type: format.media_type(),
            bytes,
        }],
        fidelity,
    };
    match format {
        ReportFormat::Html => {
            let (html, fidelity) = html::render_html(doc, options);
            Ok(single(html.into_bytes(), fidelity))
        }
        ReportFormat::Markdown => {
            let (files, fidelity) = md::render_markdown(doc, options, &stem);
            Ok(RenderOutput { files, fidelity })
        }
        ReportFormat::Xlsx => {
            let (bytes, fidelity) = xlsx::report::render_xlsx(doc, options);
            Ok(single(bytes, fidelity))
        }
        ReportFormat::Csv => {
            let (files, fidelity) = csv::render_csv(doc, options, &stem);
            if files.is_empty() {
                return Err(ReportError::Empty(format));
            }
            Ok(RenderOutput { files, fidelity })
        }
        ReportFormat::Svg => {
            let (files, fidelity) = chart_files(doc, options, &stem, false)?;
            Ok(RenderOutput { files, fidelity })
        }
        #[cfg(not(target_arch = "wasm32"))]
        ReportFormat::Pdf => {
            let (bytes, fidelity) = pdf::render_pdf(doc, options).map_err(ReportError::Render)?;
            Ok(single(bytes, fidelity))
        }
        #[cfg(not(target_arch = "wasm32"))]
        ReportFormat::Png => {
            let (files, fidelity) = chart_files(doc, options, &stem, true)?;
            Ok(RenderOutput { files, fidelity })
        }
        #[cfg(target_arch = "wasm32")]
        ReportFormat::Pdf | ReportFormat::Png => Err(ReportError::Unavailable(format)),
    }
}

/// Each chart in the document, in order, with a file-name stem.
pub fn charts(doc: &ReportDoc) -> Vec<&doc::ChartBlock> {
    let mut out = Vec::new();
    doc::walk(&doc.blocks, &mut |b| {
        if let Block::Chart(c) = b {
            out.push(c);
        }
    });
    out
}

fn chart_files(
    doc: &ReportDoc,
    options: &RenderOptions,
    stem: &str,
    png: bool,
) -> Result<(Vec<RenderedFile>, FidelityReport), ReportError> {
    let format = if png {
        ReportFormat::Png
    } else {
        ReportFormat::Svg
    };
    let theme = ReportTheme::for_mode(options.theme);
    let mut fidelity = FidelityReport::new(format);
    let mut files = Vec::new();
    let mut names = text::Anchors::default();
    for (i, chart) in charts(doc).into_iter().enumerate() {
        let rendered = svg::render_chart(&chart.spec, &chart.data, options.chart_options(theme));
        fidelity.note_chart(&rendered, chart.spec.title.as_deref());
        let label = chart
            .spec
            .title
            .clone()
            .or_else(|| chart.cell_id.clone())
            .unwrap_or_else(|| format!("chart {}", i + 1));
        let name = format!("{stem}-{}", names.next(&label));
        let markup = svg::to_svg(&rendered.scene, &name);
        if png {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let bytes =
                    png::rasterize(&markup, options.png_scale).map_err(ReportError::Render)?;
                files.push(RenderedFile {
                    path: format!("{name}.png"),
                    media_type: ReportFormat::Png.media_type(),
                    bytes,
                });
            }
        } else {
            files.push(RenderedFile {
                path: format!("{name}.svg"),
                media_type: ReportFormat::Svg.media_type(),
                bytes: markup.into_bytes(),
            });
        }
    }
    if files.is_empty() {
        return Err(ReportError::Empty(format));
    }
    fidelity.note_once(FidelityKind::StaticCharts);
    fidelity.note_once(FidelityKind::FixedTheme);
    Ok((files, fidelity))
}

#[cfg(test)]
mod tests;
