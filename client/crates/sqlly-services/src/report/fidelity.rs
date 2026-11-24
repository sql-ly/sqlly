//! What a format could not carry over, returned with every render so the
//! export dialog (and `sqlly notebook export`) can say so before the file
//! is shared (spec 10.1, fidelity contract).

use serde::{Deserialize, Serialize};
use sqlly_core::DisplayMessage;

use super::ReportFormat;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FidelityKind {
    /// Charts and tables are static: no hover, zoom or sorting.
    StaticCharts,
    /// Linked cells no longer filter each other.
    CrossFilterDropped,
    /// Charts were embedded as pictures rather than native objects.
    ChartsAsImages,
    /// Charts were left out; their data is still included.
    ChartsAsData,
    /// A chart could not be drawn (bad spec or no data).
    ChartFailed,
    /// Dashboard columns are shown one under another.
    ColumnsStacked,
    /// Tables show only the first rows.
    RowsCapped { shown: usize, total: u64 },
    /// Characters the embedded font has no glyph for (often CJK).
    GlyphsMissing { sample: String, count: usize },
    /// Scripts that need shaping (Arabic, Indic) are drawn unshaped.
    ComplexScripts,
    /// Decimals with more than 15 significant digits are rounded by Excel.
    PrecisionRounded { column: String },
    /// Integers too large for a spreadsheet number were written as text.
    LargeIntegersAsText { column: String },
    /// NaN and infinity have no spreadsheet number and were written as text.
    NonFiniteAsText { column: String },
    /// An image failed the safety checks or is not supported here.
    ImageDropped,
    /// Raw HTML in Markdown was removed.
    HtmlRemoved { count: usize },
    /// Unsafe or unsupported link targets were removed.
    LinksRemoved { count: usize },
    /// External images in Markdown were removed (no network loads).
    ExternalImagesRemoved { count: usize },
    /// Page breaks have no meaning in this format.
    PageBreaksIgnored,
    /// Long cell text was cut to fit the page.
    CellTextTruncated,
    /// Text structure (headings, KPI cards, callouts) became plain rows.
    LayoutFlattened,
    /// Rich text styling (bold, links) inside Markdown became plain text.
    RichTextFlattened,
    /// Columns beyond what fits the page width were left out.
    ColumnsOmitted { shown: usize, total: usize },
    /// The light/dark scheme is fixed in this format.
    FixedTheme,
    /// A chart had more categories than it can draw; `folded` of them are
    /// grouped under "Other".
    CategoriesCapped { folded: usize },
    /// A table or chart holds only part of its query's result (truncated,
    /// sampled, stopped early, or computed from such data).
    IncompleteData,
    /// Times in `column` carried UTC offsets, which a spreadsheet cannot
    /// hold; they were converted to UTC.
    TimezoneDropped { column: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FidelityNote {
    #[serde(flatten)]
    pub kind: FidelityKind,
    /// The block it applies to (title or cell id), when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<String>,
}

impl FidelityNote {
    pub fn message(&self) -> DisplayMessage {
        use FidelityKind as K;
        let mut message = match &self.kind {
            K::StaticCharts => DisplayMessage::new(
                "notebook.report.fidelity.static_charts",
                "Charts and tables are static: hover, zoom and sorting are not available.",
            ),
            K::CrossFilterDropped => DisplayMessage::new(
                "notebook.report.fidelity.cross_filter_dropped",
                "Linked cells no longer filter each other.",
            ),
            K::ChartsAsImages => DisplayMessage::new(
                "notebook.report.fidelity.charts_as_images",
                "Charts are embedded as pictures, not as editable charts.",
            ),
            K::ChartsAsData => DisplayMessage::new(
                "notebook.report.fidelity.charts_as_data",
                "Charts are not drawn in this format; their data is included instead.",
            ),
            K::ChartFailed => DisplayMessage::new(
                "notebook.report.fidelity.chart_failed",
                "A chart could not be drawn and is shown as a note.",
            ),
            K::ColumnsStacked => DisplayMessage::new(
                "notebook.report.fidelity.columns_stacked",
                "Dashboard columns are shown one under another.",
            ),
            K::RowsCapped { shown, total } => DisplayMessage::new(
                "notebook.report.fidelity.rows_capped",
                format!("A table shows the first {shown} of {total} rows."),
            )
            .with_arg("shown", shown.to_string())
            .with_arg("total", total.to_string()),
            K::GlyphsMissing { sample, count: 1 } => DisplayMessage::new(
                "notebook.report.fidelity.glyphs_missing_one",
                format!("1 character has no glyph in the PDF font and shows as a box ({sample})."),
            )
            .with_arg("count", "1")
            .with_arg("sample", sample.clone()),
            K::GlyphsMissing { sample, count } => DisplayMessage::new(
                "notebook.report.fidelity.glyphs_missing",
                format!("{count} characters have no glyph in the PDF font and show as boxes (for example {sample})."),
            )
            .with_arg("count", count.to_string())
            .with_arg("sample", sample.clone()),
            K::ComplexScripts => DisplayMessage::new(
                "notebook.report.fidelity.complex_scripts",
                "Right-to-left and joined scripts are drawn letter by letter and may not read correctly.",
            ),
            K::PrecisionRounded { column } => DisplayMessage::new(
                "notebook.report.fidelity.precision_rounded",
                format!("Excel keeps 15 significant digits, so some values in {column} are rounded."),
            )
            .with_arg("column", column.clone()),
            K::LargeIntegersAsText { column } => DisplayMessage::new(
                "notebook.report.fidelity.large_integers_as_text",
                format!("Integers in {column} are too large for an Excel number and were written as text."),
            )
            .with_arg("column", column.clone()),
            K::NonFiniteAsText { column } => DisplayMessage::new(
                "notebook.report.fidelity.non_finite_as_text",
                format!("NaN and infinite values in {column} were written as text."),
            )
            .with_arg("column", column.clone()),
            K::ImageDropped => DisplayMessage::new(
                "notebook.report.fidelity.image_dropped",
                "An image was left out because it is not safe or not supported in this format.",
            ),
            K::HtmlRemoved { count: 1 } => DisplayMessage::new("notebook.report.fidelity.html_removed_one", "1 piece of raw HTML in a text cell was removed.")
                .with_arg("count", "1"),
            K::HtmlRemoved { count } => DisplayMessage::new(
                "notebook.report.fidelity.html_removed",
                format!("{count} pieces of raw HTML in text cells were removed."),
            )
            .with_arg("count", count.to_string()),
            K::LinksRemoved { count: 1 } => DisplayMessage::new("notebook.report.fidelity.links_removed_one", "1 link with an unsafe target was removed.")
                .with_arg("count", "1"),
            K::LinksRemoved { count } => DisplayMessage::new(
                "notebook.report.fidelity.links_removed",
                format!("{count} links with unsafe targets were removed."),
            )
            .with_arg("count", count.to_string()),
            K::ExternalImagesRemoved { count: 1 } => DisplayMessage::new("notebook.report.fidelity.external_images_removed_one", "1 image that would load from the internet was removed.")
                .with_arg("count", "1"),
            K::ExternalImagesRemoved { count } => DisplayMessage::new(
                "notebook.report.fidelity.external_images_removed",
                format!("{count} images that would load from the internet were removed."),
            )
            .with_arg("count", count.to_string()),
            K::PageBreaksIgnored => DisplayMessage::new(
                "notebook.report.fidelity.page_breaks_ignored",
                "Page breaks are ignored in this format.",
            ),
            K::CellTextTruncated => DisplayMessage::new(
                "notebook.report.fidelity.cell_text_truncated",
                "Long cell text was shortened to fit the page.",
            ),
            K::LayoutFlattened => DisplayMessage::new(
                "notebook.report.fidelity.layout_flattened",
                "Headings, cards and notes are listed as plain rows.",
            ),
            K::RichTextFlattened => DisplayMessage::new(
                "notebook.report.fidelity.rich_text_flattened",
                "Text formatting such as bold and links is shown as plain text.",
            ),
            K::ColumnsOmitted { shown, total } => DisplayMessage::new(
                "notebook.report.fidelity.columns_omitted",
                format!("A table shows {shown} of its {total} columns to fit the page."),
            )
            .with_arg("shown", shown.to_string())
            .with_arg("total", total.to_string()),
            K::FixedTheme => DisplayMessage::new(
                "notebook.report.fidelity.fixed_theme",
                "Colors are fixed to one theme and do not follow light or dark mode.",
            ),
            K::CategoriesCapped { folded } => DisplayMessage::new(
                "notebook.report.fidelity.categories_capped",
                format!("A chart has too many categories to draw; {folded} of them are grouped under Other."),
            )
            .with_arg("count", folded.to_string()),
            K::IncompleteData => DisplayMessage::new(
                "notebook.report.fidelity.incomplete_data",
                "Some tables or charts hold only part of their query's result (truncated, sampled or stopped early).",
            ),
            K::TimezoneDropped { column } => DisplayMessage::new(
                "notebook.report.fidelity.timezone_dropped",
                format!("Excel has no time zones, so times with an offset in {column} were converted to UTC."),
            )
            .with_arg("column", column.clone()),
        };
        if let Some(block) = &self.block {
            message = message.with_arg("block", block.clone());
        }
        message
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FidelityReport {
    pub format: ReportFormat,
    pub notes: Vec<FidelityNote>,
}

impl FidelityReport {
    pub fn new(format: ReportFormat) -> Self {
        Self {
            format,
            notes: Vec::new(),
        }
    }

    /// Adds a note unless the same one is already there.
    pub fn note(&mut self, kind: FidelityKind, block: Option<&str>) {
        let note = FidelityNote {
            kind,
            block: block.map(str::to_string),
        };
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    /// Adds a document-wide note once, whatever block raised it.
    pub fn note_once(&mut self, kind: FidelityKind) {
        if !self.notes.iter().any(|n| n.kind == kind) {
            self.notes.push(FidelityNote { kind, block: None });
        }
    }

    /// Notes for a drawn chart: a failed drawing, or categories folded into
    /// "Other".
    pub fn note_chart(&mut self, rendered: &super::svg::RenderedChart, block: Option<&str>) {
        if rendered.error.is_some() {
            self.note(FidelityKind::ChartFailed, block);
        }
        if rendered.capped_categories > 0 {
            self.note(
                FidelityKind::CategoriesCapped {
                    folded: rendered.capped_categories,
                },
                block,
            );
        }
    }

    /// A note when `table` holds only part of its result.
    pub fn note_completeness(&mut self, table: &super::doc::TableBlock, block: Option<&str>) {
        use super::doc::Completeness as C;
        let partial = match table.completeness {
            C::Complete => false,
            C::Spilled { total } => (table.rows.len() as u64) < total,
            _ => true,
        };
        if partial {
            self.note(FidelityKind::IncompleteData, block);
        }
    }

    pub fn has(&self, pred: impl Fn(&FidelityKind) -> bool) -> bool {
        self.notes.iter().any(|n| pred(&n.kind))
    }

    pub fn is_lossless(&self) -> bool {
        self.notes.is_empty()
    }

    pub fn absorb_markdown(&mut self, removed: super::markdown::Removed) {
        if removed.html > 0 {
            self.bump(
                |k| matches!(k, FidelityKind::HtmlRemoved { .. }),
                FidelityKind::HtmlRemoved {
                    count: removed.html,
                },
            );
        }
        if removed.links > 0 {
            self.bump(
                |k| matches!(k, FidelityKind::LinksRemoved { .. }),
                FidelityKind::LinksRemoved {
                    count: removed.links,
                },
            );
        }
        if removed.images > 0 {
            self.bump(
                |k| matches!(k, FidelityKind::ExternalImagesRemoved { .. }),
                FidelityKind::ExternalImagesRemoved {
                    count: removed.images,
                },
            );
        }
    }

    fn bump(&mut self, matches: impl Fn(&FidelityKind) -> bool, fresh: FidelityKind) {
        if let Some(note) = self.notes.iter_mut().find(|n| matches(&n.kind)) {
            match (&mut note.kind, fresh) {
                (FidelityKind::HtmlRemoved { count }, FidelityKind::HtmlRemoved { count: add })
                | (
                    FidelityKind::LinksRemoved { count },
                    FidelityKind::LinksRemoved { count: add },
                )
                | (
                    FidelityKind::ExternalImagesRemoved { count },
                    FidelityKind::ExternalImagesRemoved { count: add },
                ) => *count += add,
                _ => {}
            }
        } else {
            self.notes.push(FidelityNote {
                kind: fresh,
                block: None,
            });
        }
    }
}
