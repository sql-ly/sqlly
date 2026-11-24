//! Markdown export: GitHub-flavored tables, fenced SQL, and each chart as
//! an SVG asset file (or an inline `data:` image) next to the document.

use base64::Engine;
use sqlly_core::CellValue;

use super::doc::{Block, ImageData, ReportDoc, TableBlock};
use super::fidelity::{FidelityKind, FidelityReport};
use super::format::format_cell;
use super::labels::ReportLabels;
use super::markdown::{self, Align, Inline, MdBlock};
use super::svg;
use super::text::{Anchors, one_line, png_is_acceptable, svg_is_unsafe};
use super::theme::ReportTheme;
use super::{MarkdownImages, RenderOptions, RenderedFile, ReportFormat, plan};

/// Escapes text so Markdown reads it literally.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.chars().enumerate() {
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '~' => {
                out.push('\\');
                out.push(c);
            }
            '#' | '+' | '-' | '=' | '!' if i == 0 => {
                out.push('\\');
                out.push(c);
            }
            '&' => out.push_str("&amp;"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

fn fence(code: &str) -> String {
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat((longest + 1).max(3))
}

struct Writer<'a> {
    doc: &'a ReportDoc,
    options: &'a RenderOptions,
    labels: &'a ReportLabels,
    out: String,
    assets: Vec<RenderedFile>,
    stem: &'a str,
    names: Anchors,
    fidelity: FidelityReport,
}

pub fn render_markdown(
    doc: &ReportDoc,
    options: &RenderOptions,
    stem: &str,
) -> (Vec<RenderedFile>, FidelityReport) {
    let mut w = Writer {
        doc,
        options,
        labels: &options.labels,
        out: String::new(),
        assets: Vec::new(),
        stem,
        names: Anchors::default(),
        fidelity: FidelityReport::new(ReportFormat::Markdown),
    };
    w.out
        .push_str(&format!("# {}\n\n", escape(&doc.meta.title)));
    if let Some(s) = doc.meta.subtitle.as_deref().filter(|s| !s.is_empty()) {
        w.out.push_str(&format!("_{}_\n\n", escape(s)));
    }
    if doc.meta.layout.cover {
        let lines = plan::meta_lines(doc, w.labels);
        for (label, value) in &lines {
            w.out
                .push_str(&format!("- **{}:** {}\n", escape(label), escape(value)));
        }
        if !lines.is_empty() {
            w.out.push('\n');
        }
    }
    for block in &doc.blocks {
        w.block(block);
    }
    let sql = plan::appendix_sql(doc);
    if !sql.is_empty() {
        w.out
            .push_str(&format!("## {}\n\n", escape(&w.labels.appendix_sql)));
        for c in sql {
            w.code(c);
        }
    }
    let provenance = plan::appendix_provenance(doc);
    if !provenance.is_empty() {
        w.out
            .push_str(&format!("## {}\n\n", escape(&w.labels.appendix_provenance)));
        for p in provenance {
            w.provenance(p);
        }
    }
    let text = w.out.trim_end().to_string() + "\n";
    let mut files = vec![RenderedFile {
        path: format!("{stem}.md"),
        media_type: ReportFormat::Markdown.media_type(),
        bytes: text.into_bytes(),
    }];
    files.extend(w.assets);
    (files, w.fidelity)
}

impl Writer<'_> {
    fn block(&mut self, block: &Block) {
        if !plan::inline(block, self.doc) {
            return;
        }
        match block {
            Block::Heading(h) => {
                let level = (h.level() + 1).min(6) as usize;
                self.out
                    .push_str(&format!("{} {}\n\n", "#".repeat(level), escape(&h.text)));
            }
            Block::Markdown(m) => {
                let parsed = markdown::parse(&m.source);
                self.fidelity.absorb_markdown(parsed.removed);
                self.md_blocks(&parsed.blocks, "");
            }
            Block::Table(t) => self.table(t),
            Block::Chart(c) => {
                let theme = ReportTheme::for_mode(self.options.theme);
                let rendered =
                    svg::render_chart(&c.spec, &c.data, self.options.chart_options(theme));
                self.fidelity.note_chart(&rendered, c.spec.title.as_deref());
                self.fidelity.note_once(FidelityKind::StaticCharts);
                self.fidelity.note_once(FidelityKind::FixedTheme);
                let title = c
                    .spec
                    .title
                    .clone()
                    .or_else(|| c.cell_id.clone())
                    .unwrap_or_else(|| "chart".into());
                let name = format!("{}-{}", self.stem, self.names.next(&title));
                let markup = svg::to_svg(&rendered.scene, &name);
                let target = self.asset(&name, "svg", markup.into_bytes(), "image/svg+xml");
                self.out
                    .push_str(&format!("![{}]({target})\n\n", escape(&one_line(&title))));
                if let Some(caption) = c.caption.as_deref().filter(|c| !c.is_empty()) {
                    self.out.push_str(&format!("_{}_\n\n", escape(caption)));
                }
            }
            Block::Kpi(k) => {
                let mut line = format!(
                    "**{}:** {}",
                    escape(&k.label),
                    escape(&svg::kpi_value_text(k))
                );
                if let Some(delta) = svg::kpi_delta_text(k, self.labels) {
                    line.push_str(&format!(" ({})", escape(&delta)));
                }
                if let Some(s) = k.resolved_status() {
                    line.push_str(&format!(" · {}", escape(self.labels.status(s))));
                }
                if let Some(caption) = k.caption.as_deref().filter(|c| !c.is_empty()) {
                    line.push_str(&format!(" · _{}_", escape(caption)));
                }
                self.out.push_str(&format!("{line}\n\n"));
                self.fidelity.note_once(FidelityKind::LayoutFlattened);
            }
            Block::Callout(c) => {
                let mut head = format!("> **{}**", escape(self.labels.callout(c.kind)));
                if let Some(t) = c.title.as_deref().filter(|t| !t.is_empty()) {
                    head.push_str(&format!(" · {}", escape(t)));
                }
                self.out.push_str(&head);
                self.out.push_str("\n>\n");
                let parsed = markdown::parse(&c.text);
                self.fidelity.absorb_markdown(parsed.removed);
                self.md_blocks(&parsed.blocks, "> ");
                if !c.evidence.is_empty() {
                    self.out.push_str(&format!(
                        "> _{}: {}_\n",
                        escape(&self.labels.evidence),
                        escape(&c.evidence.join(", "))
                    ));
                }
                self.out.push('\n');
            }
            Block::Assertion(a) => {
                let mut line = format!(
                    "- **{}** {}",
                    escape(self.labels.assertion(a.status)),
                    escape(&a.name)
                );
                if let Some(e) = a.expected.as_deref() {
                    line.push_str(&format!(
                        " · {}: {}",
                        escape(&self.labels.expected),
                        escape(e)
                    ));
                }
                if let Some(v) = a.actual.as_deref() {
                    line.push_str(&format!(
                        " · {}: {}",
                        escape(&self.labels.actual),
                        escape(v)
                    ));
                }
                if let Some(m) = a.message.as_deref().filter(|m| !m.is_empty()) {
                    line.push_str(&format!(" · {}", escape(m)));
                }
                self.out.push_str(&format!("{line}\n\n"));
            }
            Block::Code(c) => self.code(c),
            Block::Provenance(p) => self.provenance(p),
            Block::PageBreak => {
                self.out.push_str("---\n\n");
                self.fidelity.note_once(FidelityKind::PageBreaksIgnored);
            }
            Block::Section(s) => {
                if let Some(t) = s.title.as_deref().filter(|t| !t.is_empty()) {
                    self.out.push_str(&format!("## {}\n\n", escape(t)));
                }
                for b in &s.blocks {
                    self.block(b);
                }
            }
            Block::Columns(cols) => {
                if cols.columns.len() > 1 {
                    self.fidelity.note_once(FidelityKind::ColumnsStacked);
                }
                for col in &cols.columns {
                    for b in &col.blocks {
                        self.block(b);
                    }
                }
            }
            Block::Image(img) => {
                let alt = escape(&one_line(&img.alt));
                let name = format!(
                    "{}-{}",
                    self.stem,
                    self.names.next(img.caption.as_deref().unwrap_or("image"))
                );
                let target = match &img.image {
                    ImageData::Svg { markup } if !svg_is_unsafe(markup) => {
                        Some(self.asset(&name, "svg", markup.clone().into_bytes(), "image/svg+xml"))
                    }
                    ImageData::Png { bytes } if png_is_acceptable(bytes) => {
                        Some(self.asset(&name, "png", bytes.clone(), "image/png"))
                    }
                    _ => None,
                };
                match target {
                    Some(t) => self.out.push_str(&format!("![{alt}]({t})\n\n")),
                    None => {
                        self.fidelity
                            .note(FidelityKind::ImageDropped, img.caption.as_deref());
                        self.out
                            .push_str(&format!("_{}_\n\n", escape(&self.labels.image_unavailable)));
                    }
                }
            }
        }
    }

    /// Stores an asset and returns the link target for it.
    fn asset(&mut self, name: &str, ext: &str, bytes: Vec<u8>, media: &'static str) -> String {
        match self.options.markdown_images {
            MarkdownImages::Inline => format!(
                "data:{media};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            ),
            MarkdownImages::Files => {
                let path = format!("assets/{name}.{ext}");
                self.assets.push(RenderedFile {
                    path: path.clone(),
                    media_type: media,
                    bytes,
                });
                path
            }
        }
    }

    fn table(&mut self, t: &TableBlock) {
        if let Some(title) = t.title.as_deref().filter(|t| !t.is_empty()) {
            self.out.push_str(&format!("**{}**", escape(title)));
            if let Some(b) = self.labels.completeness(t.completeness) {
                self.out.push_str(&format!(" _({})_", escape(&b)));
            }
            self.out.push_str("\n\n");
        } else if let Some(b) = self.labels.completeness(t.completeness) {
            self.out.push_str(&format!("_({})_\n\n", escape(&b)));
        }
        if t.columns.is_empty() {
            return;
        }
        let kinds: Vec<_> = t.columns.iter().map(|c| c.kind()).collect();
        let header: Vec<String> = t
            .columns
            .iter()
            .map(|c| escape(&one_line(&c.name)))
            .collect();
        self.out.push_str(&format!("| {} |\n", header.join(" | ")));
        let rule: Vec<&str> = kinds
            .iter()
            .map(|k| if k.is_right_aligned() { "---:" } else { "---" })
            .collect();
        self.out.push_str(&format!("| {} |\n", rule.join(" | ")));
        let shown = t.shown_rows(self.doc.meta.layout.table_row_cap);
        for row in &t.rows[..shown] {
            let cells: Vec<String> = t
                .columns
                .iter()
                .zip(&kinds)
                .enumerate()
                .map(|(i, (c, k))| {
                    escape(&one_line(&format_cell(
                        row.get(i).unwrap_or(&CellValue::Null),
                        *k,
                        &c.format,
                    )))
                })
                .collect();
            self.out.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
        self.out.push('\n');
        if t.data_withheld {
            self.out
                .push_str(&format!("_{}_\n\n", escape(&self.labels.data_withheld)));
        } else if shown < t.rows.len() || t.source_rows() > shown as u64 {
            self.out.push_str(&format!(
                "_{}_\n\n",
                escape(&self.labels.shown(shown, t.source_rows()))
            ));
        }
        if shown < t.rows.len() {
            self.fidelity.note(
                FidelityKind::RowsCapped {
                    shown,
                    total: t.source_rows(),
                },
                t.title.as_deref(),
            );
        }
        if let Some(c) = t.caption.as_deref().filter(|c| !c.is_empty()) {
            self.out.push_str(&format!("_{}_\n\n", escape(c)));
        }
    }

    fn code(&mut self, c: &super::doc::CodeBlock) {
        if let Some(cap) = c.caption.as_deref().or(c.cell_id.as_deref()) {
            self.out.push_str(&format!("**{}**\n\n", escape(cap)));
        }
        let f = fence(&c.source);
        let lang: String = c
            .language
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .take(16)
            .collect();
        self.out.push_str(&format!(
            "{f}{lang}\n{}\n{f}\n\n",
            c.source.trim_end_matches('\n')
        ));
    }

    fn provenance(&mut self, p: &super::doc::Provenance) {
        for (label, value) in plan::provenance_lines(p, self.labels) {
            self.out.push_str(&format!(
                "- {}: `{}`\n",
                escape(&label),
                value.replace('`', "'")
            ));
        }
        self.out.push('\n');
    }

    fn md_blocks(&mut self, blocks: &[MdBlock], prefix: &str) {
        for block in blocks {
            let text = match block {
                MdBlock::Paragraph(c) => inlines(c),
                MdBlock::Heading { level, content } => {
                    format!(
                        "{} {}",
                        "#".repeat(((*level + 1).min(6)) as usize),
                        inlines(content)
                    )
                }
                MdBlock::List {
                    ordered,
                    start,
                    items,
                } => {
                    let mut lines = Vec::new();
                    for (i, item) in items.iter().enumerate() {
                        let marker = if *ordered {
                            format!("{}.", *start as usize + i)
                        } else {
                            "-".into()
                        };
                        let check = match item.checked {
                            Some(true) => "[x] ",
                            Some(false) => "[ ] ",
                            None => "",
                        };
                        let body = markdown::plain_blocks(&item.blocks);
                        let body = match item.blocks.as_slice() {
                            [MdBlock::Paragraph(c)] => inlines(c),
                            _ => escape(&one_line(&body)),
                        };
                        lines.push(format!("{marker} {check}{body}"));
                    }
                    lines.join("\n")
                }
                MdBlock::Quote(inner) => {
                    let mut nested = Writer {
                        doc: self.doc,
                        options: self.options,
                        labels: self.labels,
                        out: String::new(),
                        assets: Vec::new(),
                        stem: self.stem,
                        names: Anchors::default(),
                        fidelity: FidelityReport::new(ReportFormat::Markdown),
                    };
                    nested.md_blocks(inner, "> ");
                    nested.out.trim_end().to_string()
                }
                MdBlock::Code { language, text } => {
                    let f = fence(text);
                    format!(
                        "{f}{}\n{}\n{f}",
                        language.as_deref().unwrap_or(""),
                        text.trim_end_matches('\n')
                    )
                }
                MdBlock::Table {
                    align,
                    header,
                    rows,
                } => {
                    let mut lines = vec![format!(
                        "| {} |",
                        header
                            .iter()
                            .map(|c| inlines(c))
                            .collect::<Vec<_>>()
                            .join(" | ")
                    )];
                    lines.push(format!(
                        "| {} |",
                        (0..header.len())
                            .map(|i| match align.get(i) {
                                Some(Align::Right) => "---:",
                                Some(Align::Center) => ":---:",
                                Some(Align::Left) => ":---",
                                _ => "---",
                            })
                            .collect::<Vec<_>>()
                            .join(" | ")
                    ));
                    for row in rows {
                        lines.push(format!(
                            "| {} |",
                            row.iter()
                                .map(|c| inlines(c))
                                .collect::<Vec<_>>()
                                .join(" | ")
                        ));
                    }
                    lines.join("\n")
                }
                MdBlock::Rule => "---".into(),
            };
            for line in text.lines() {
                if line.is_empty() {
                    self.out.push_str(prefix.trim_end());
                } else {
                    self.out.push_str(prefix);
                    self.out.push_str(line);
                }
                self.out.push('\n');
            }
            self.out.push_str(prefix.trim_end());
            self.out.push('\n');
        }
    }
}

fn inlines(content: &[Inline]) -> String {
    let mut out = String::new();
    for inline in content {
        match inline {
            Inline::Text(t) => out.push_str(&escape_inline(t)),
            Inline::Strong(c) => out.push_str(&format!("**{}**", inlines(c))),
            Inline::Emphasis(c) => out.push_str(&format!("_{}_", inlines(c))),
            Inline::Strike(c) => out.push_str(&format!("~~{}~~", inlines(c))),
            Inline::Code(t) => {
                let ticks = if t.contains('`') { "``" } else { "`" };
                out.push_str(&format!("{ticks}{t}{ticks}"));
            }
            Inline::Link { url, content } => out.push_str(&format!(
                "[{}](<{}>)",
                inlines(content),
                url.replace(['<', '>'], "")
            )),
            Inline::Image { src, alt } => {
                out.push_str(&format!("![{}]({src})", escape_inline(alt)))
            }
            Inline::Break => out.push_str("  \n"),
        }
    }
    out
}

/// Like [`escape`] but for text in the middle of a line.
fn escape_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '~' => {
                out.push('\\');
                out.push(c);
            }
            '&' => out.push_str("&amp;"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_neutralises_markup() {
        assert_eq!(
            escape("<script>alert(1)</script>"),
            "\\<script\\>alert(1)\\</script\\>"
        );
        assert_eq!(escape("# not a heading"), "\\# not a heading");
        assert_eq!(escape("a|b"), "a\\|b");
        assert_eq!(fence("a ``` b"), "````");
    }
}
