//! A single self-contained HTML file: inline CSS, inline SVG charts, no
//! JavaScript and a Content-Security-Policy that forbids loading anything,
//! light and dark through `prefers-color-scheme`, and a print stylesheet
//! (light colors, repeated table headers, sensible page breaks). Every
//! string from the notebook is escaped.

use base64::Engine;
use sqlly_core::CellValue;

use super::doc::{
    AssertionResult, Block, Callout, CodeBlock, ImageBlock, ImageData, KpiBlock, ReportDoc,
    TableBlock,
};
use super::fidelity::{FidelityKind, FidelityReport};
use super::format::format_cell;
use super::labels::ReportLabels;
use super::markdown::{self, Align, Inline, MdBlock};
use super::svg::{self, scene::to_svg};
use super::text::{Anchors, html_escape as esc, png_is_acceptable, svg_is_unsafe};
use super::theme::{DARK, LIGHT, ReportTheme};
use super::{RenderOptions, ReportFormat, plan};

struct Writer<'a> {
    doc: &'a ReportDoc,
    options: &'a RenderOptions,
    labels: &'a ReportLabels,
    out: String,
    anchors: Anchors,
    toc: Vec<(u8, String, String)>,
    fidelity: FidelityReport,
    figure: usize,
}

pub fn render_html(doc: &ReportDoc, options: &RenderOptions) -> (String, FidelityReport) {
    let mut w = Writer {
        doc,
        options,
        labels: &options.labels,
        out: String::new(),
        anchors: Anchors::default(),
        toc: Vec::new(),
        fidelity: FidelityReport::new(ReportFormat::Html),
        figure: 0,
    };
    for block in &doc.blocks {
        w.block(block);
    }
    w.appendix();
    let body = std::mem::take(&mut w.out);

    let mut page = String::new();
    page.push_str("<!DOCTYPE html>\n");
    page.push_str(&format!(
        "<html lang=\"{}\">\n<head>\n",
        esc(&lang(&options.language))
    ));
    page.push_str("<meta charset=\"utf-8\">\n");
    page.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    page.push_str("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\">\n");
    page.push_str("<meta name=\"color-scheme\" content=\"light dark\">\n");
    page.push_str("<meta name=\"generator\" content=\"SQLLY\">\n");
    page.push_str(&format!("<title>{}</title>\n", esc(&doc.meta.title)));
    page.push_str("<style>\n");
    page.push_str(&stylesheet());
    page.push_str("</style>\n</head>\n<body>\n");
    page.push_str(&cover(doc, &options.labels));
    if doc.meta.layout.contents && w.toc.len() >= 3 {
        page.push_str(&format!(
            "<nav class=\"toc\" aria-labelledby=\"toc-title\">\n<h2 id=\"toc-title\">{}</h2>\n<ol>\n",
            esc(&options.labels.contents)
        ));
        for (level, text, id) in &w.toc {
            page.push_str(&format!(
                "<li class=\"toc-{}\"><a href=\"#{}\">{}</a></li>\n",
                level.min(&3),
                esc(id),
                esc(text)
            ));
        }
        page.push_str("</ol>\n</nav>\n");
    }
    page.push_str("<main>\n");
    page.push_str(&body);
    page.push_str("</main>\n");
    let mut footer = Vec::new();
    if let Some(g) = doc.meta.generated_at.as_deref().filter(|g| !g.is_empty()) {
        footer.push(format!("{} {}", esc(&options.labels.generated), esc(g)));
    }
    if let Some(v) = doc
        .meta
        .notebook_version
        .as_deref()
        .filter(|v| !v.is_empty())
    {
        footer.push(format!("{} {}", esc(&options.labels.version), esc(v)));
    }
    page.push_str(&format!("<footer>{}</footer>\n", footer.join(" · ")));
    page.push_str("</body>\n</html>\n");
    (page, w.fidelity)
}

/// A BCP 47 tag with only letters, digits and hyphens.
fn lang(code: &str) -> String {
    let clean: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(35)
        .collect();
    if clean.is_empty() { "en".into() } else { clean }
}

fn cover(doc: &ReportDoc, labels: &ReportLabels) -> String {
    let mut out = String::from("<header class=\"cover\">\n");
    out.push_str(&format!("<h1>{}</h1>\n", esc(&doc.meta.title)));
    if let Some(s) = doc.meta.subtitle.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("<p class=\"subtitle\">{}</p>\n", esc(s)));
    }
    let lines = plan::meta_lines(doc, labels);
    if doc.meta.layout.cover && !lines.is_empty() {
        out.push_str("<dl class=\"meta\">\n");
        for (label, value) in lines {
            out.push_str(&format!(
                "<div><dt>{}</dt><dd>{}</dd></div>\n",
                esc(&label),
                esc(&value)
            ));
        }
        out.push_str("</dl>\n");
    }
    out.push_str("</header>\n");
    out
}

fn css_vars(t: &ReportTheme) -> String {
    let series: String = t
        .series
        .iter()
        .enumerate()
        .map(|(i, c)| format!("--s{i}:{};", c.css()))
        .collect();
    format!(
        "--bg:{};--surface:{};--text:{};--dim:{};--border:{};--grid:{};--accent:{};--good:{};--warn:{};--bad:{};{series}",
        t.background.css(),
        t.surface.css(),
        t.text.css(),
        t.text_dim.css(),
        t.border.css(),
        t.grid.css(),
        t.accent.css(),
        t.good.css(),
        t.warning.css(),
        t.critical.css()
    )
}

fn stylesheet() -> String {
    let mut css = String::new();
    css.push_str(&format!(
        ":root{{{}color-scheme:light dark;}}\n",
        css_vars(&LIGHT)
    ));
    css.push_str(&format!(
        "@media (prefers-color-scheme: dark){{:root{{{}}}.chart-light{{display:none}}.chart-dark{{display:block}}}}\n",
        css_vars(&DARK)
    ));
    css.push_str(r#"*{box-sizing:border-box}
body{margin:0 auto;max-width:1040px;padding:32px 24px 48px;background:var(--bg);color:var(--text);font:15px/1.55 'Noto Sans',-apple-system,'Segoe UI','Helvetica Neue',Arial,sans-serif}
h1{font-size:28px;line-height:1.2;margin:0 0 6px}
h2{font-size:21px;margin:32px 0 10px}h3{font-size:17px;margin:24px 0 8px}h4,h5,h6{font-size:15px;margin:18px 0 6px}
a{color:var(--accent)}
p{margin:8px 0}
.cover{border-bottom:1px solid var(--border);padding-bottom:16px;margin-bottom:20px}
.subtitle{color:var(--dim);font-size:17px;margin:0 0 12px}
.meta{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:6px 18px;margin:12px 0 0}
.meta div{display:flex;gap:8px}.meta dt{color:var(--dim);min-width:96px}.meta dd{margin:0;overflow-wrap:anywhere}
.toc{background:var(--surface);border:1px solid var(--border);border-radius:8px;padding:8px 18px;margin:0 0 24px}
.toc h2{font-size:15px;margin:8px 0}.toc ol{margin:0 0 8px;padding-left:20px}.toc-2{margin-left:14px}.toc-3{margin-left:28px}
figure{margin:18px 0}
figcaption{font-weight:600;margin-bottom:6px}
.badge{display:inline-block;font-size:12px;font-weight:600;border:1px solid var(--warn);color:var(--warn);border-radius:10px;padding:0 8px;margin-left:8px;vertical-align:middle}
.scroll{overflow-x:auto;border:1px solid var(--border);border-radius:6px}
table{border-collapse:collapse;width:100%;font-size:13px}
th,td{padding:5px 10px;border-bottom:1px solid var(--grid);text-align:left;vertical-align:top;overflow-wrap:anywhere}
th{background:var(--surface);font-weight:600;position:sticky;top:0}
td.num,th.num{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
td.null{color:var(--dim)}
tbody tr:nth-child(even){background:var(--surface)}
.note{color:var(--dim);font-size:13px}
.withheld{color:var(--dim);font-style:italic}
.chart svg,.kpi svg,.image img{max-width:100%;height:auto;display:block}
.chart-dark{display:none}
.kpi{background:var(--surface);border:1px solid var(--border);border-left:4px solid var(--border);border-radius:8px;padding:12px 14px;margin:10px 0}
.kpi.good{border-left-color:var(--good)}.kpi.warning{border-left-color:var(--warn)}.kpi.critical{border-left-color:var(--bad)}
.kpi-label{color:var(--dim);font-size:13px}.kpi-value{font-size:28px;font-weight:700;line-height:1.2;font-variant-numeric:tabular-nums}
.kpi-delta,.kpi-status{font-size:13px}.up{color:var(--good)}.down{color:var(--bad)}.flat{color:var(--dim)}
.kpi-status.good{color:var(--good)}.kpi-status.warning{color:var(--warn)}.kpi-status.critical{color:var(--bad)}
.callout{border-left:4px solid var(--dim);background:var(--surface);border-radius:0 8px 8px 0;padding:8px 14px;margin:14px 0}
.callout-kind{font-size:12px;font-weight:700;text-transform:uppercase;letter-spacing:.04em;margin:0}
.callout.fact{border-left-color:var(--accent)}.callout.derived{border-left-color:var(--s5)}.callout.hypothesis{border-left-color:var(--s3)}
.callout.recommendation{border-left-color:var(--good)}.callout.warning{border-left-color:var(--warn)}
.assertion{display:flex;flex-wrap:wrap;gap:4px 10px;align-items:baseline;padding:8px 0;border-bottom:1px solid var(--grid)}
.pill{font-size:12px;font-weight:700;border-radius:10px;padding:1px 9px;color:var(--bg);background:var(--dim)}
.pill.passed{background:var(--good)}.pill.failed,.pill.error{background:var(--bad)}.pill.warning{background:var(--warn)}
pre{background:var(--surface);border:1px solid var(--border);border-radius:6px;padding:10px 12px;overflow-x:auto;font:13px/1.45 'JetBrains Mono',ui-monospace,Menlo,Consolas,monospace;white-space:pre}
code{font-family:'JetBrains Mono',ui-monospace,Menlo,Consolas,monospace;font-size:.92em}
blockquote{border-left:3px solid var(--border);margin:10px 0;padding:2px 14px;color:var(--dim)}
.provenance{display:grid;grid-template-columns:max-content 1fr;gap:2px 14px;font-size:13px;color:var(--dim);margin:10px 0}
.provenance dt{font-weight:600}.provenance dd{margin:0;overflow-wrap:anywhere}
.grid{display:grid;grid-template-columns:repeat(12,minmax(0,1fr));gap:14px;margin:14px 0}
.grid>div{min-width:0}
.span-1{grid-column:span 1}.span-2{grid-column:span 2}.span-3{grid-column:span 3}.span-4{grid-column:span 4}.span-5{grid-column:span 5}.span-6{grid-column:span 6}
.span-7{grid-column:span 7}.span-8{grid-column:span 8}.span-9{grid-column:span 9}.span-10{grid-column:span 10}.span-11{grid-column:span 11}.span-12{grid-column:span 12}
.appendix{border-top:1px solid var(--border);margin-top:36px}
footer{color:var(--dim);font-size:12px;border-top:1px solid var(--border);margin-top:36px;padding-top:10px}
@media (max-width:720px){.grid{grid-template-columns:1fr}.grid>div{grid-column:auto}body{padding:20px 16px}}
"#);
    css.push_str(&format!(
        "@media print{{:root{{{}}}body{{max-width:none;padding:0;font-size:11pt;background:#fff}}.chart-light{{display:block!important}}.chart-dark{{display:none!important}}.scroll{{overflow:visible;border:none}}th{{position:static}}thead{{display:table-header-group}}tr,figure,.kpi,.callout,.assertion,pre{{break-inside:avoid}}h2,h3{{break-after:avoid}}.page-break{{break-after:page}}.toc{{break-after:page}}a{{color:inherit}}}}\n",
        css_vars(&LIGHT)
    ));
    css.push_str("@page{margin:18mm 16mm}\n");
    css
}

impl Writer<'_> {
    fn heading(&mut self, level: u8, text: &str) {
        let id = self.anchors.next(text);
        let h = (level + 1).min(6);
        self.out
            .push_str(&format!("<h{h} id=\"{}\">{}</h{h}>\n", esc(&id), esc(text)));
        if level <= 3 {
            self.toc.push((level, text.to_string(), id));
        }
    }

    fn block(&mut self, block: &Block) {
        if !plan::inline(block, self.doc) {
            return;
        }
        match block {
            Block::Heading(h) => self.heading(h.level(), &h.text),
            Block::Markdown(m) => {
                let parsed = markdown::parse(&m.source);
                self.fidelity.absorb_markdown(parsed.removed);
                self.md_blocks(&parsed.blocks);
            }
            Block::Table(t) => self.table(t),
            Block::Chart(c) => self.chart(c),
            Block::Kpi(k) => self.kpi(k),
            Block::Callout(c) => self.callout(c),
            Block::Assertion(a) => self.assertion(a),
            Block::Code(c) => self.code(c),
            Block::Provenance(p) => self.provenance(p),
            Block::PageBreak => self.out.push_str("<div class=\"page-break\"></div>\n"),
            Block::Section(s) => {
                self.out.push_str("<section>\n");
                if let Some(t) = s.title.as_deref().filter(|t| !t.is_empty()) {
                    self.heading(1, t);
                }
                for b in &s.blocks {
                    self.block(b);
                }
                self.out.push_str("</section>\n");
            }
            Block::Columns(cols) => {
                self.out.push_str("<div class=\"grid\">\n");
                for col in &cols.columns {
                    self.out
                        .push_str(&format!("<div class=\"span-{}\">\n", col.span()));
                    for b in &col.blocks {
                        self.block(b);
                    }
                    self.out.push_str("</div>\n");
                }
                self.out.push_str("</div>\n");
            }
            Block::Image(img) => self.image(img),
        }
    }

    fn table(&mut self, t: &TableBlock) {
        self.out.push_str("<figure class=\"table\">\n");
        let badge = self.labels.completeness(t.completeness);
        if t.title.is_some() || badge.is_some() {
            self.out.push_str("<figcaption>");
            if let Some(title) = t.title.as_deref() {
                self.out.push_str(&esc(title));
            }
            if let Some(b) = badge {
                self.out
                    .push_str(&format!("<span class=\"badge\">{}</span>", esc(&b)));
            }
            self.out.push_str("</figcaption>\n");
        }
        let kinds: Vec<_> = t.columns.iter().map(|c| c.kind()).collect();
        self.out
            .push_str("<div class=\"scroll\"><table>\n<thead><tr>");
        for (c, kind) in t.columns.iter().zip(&kinds) {
            let class = if kind.is_right_aligned() {
                " class=\"num\""
            } else {
                ""
            };
            self.out
                .push_str(&format!("<th scope=\"col\"{class}>{}</th>", esc(&c.name)));
        }
        self.out.push_str("</tr></thead>\n<tbody>\n");
        let shown = t.shown_rows(self.doc.meta.layout.table_row_cap);
        for row in &t.rows[..shown] {
            self.out.push_str("<tr>");
            for (i, (column, kind)) in t.columns.iter().zip(&kinds).enumerate() {
                let value = row.get(i).unwrap_or(&CellValue::Null);
                let text = format_cell(value, *kind, &column.format);
                let mut classes = Vec::new();
                if kind.is_right_aligned() {
                    classes.push("num");
                }
                if matches!(value, CellValue::Null) {
                    classes.push("null");
                }
                let class = if classes.is_empty() {
                    String::new()
                } else {
                    format!(" class=\"{}\"", classes.join(" "))
                };
                self.out
                    .push_str(&format!("<td{class}>{}</td>", esc(&text)));
            }
            self.out.push_str("</tr>\n");
        }
        self.out.push_str("</tbody>\n</table></div>\n");
        if t.data_withheld {
            self.out.push_str(&format!(
                "<p class=\"withheld\">{}</p>\n",
                esc(&self.labels.data_withheld)
            ));
        } else if t.rows.is_empty() {
            self.out.push_str(&format!(
                "<p class=\"note\">{}</p>\n",
                esc(&self.labels.no_rows)
            ));
        } else if shown < t.rows.len() || (t.source_rows() > shown as u64) {
            self.out.push_str(&format!(
                "<p class=\"note\">{}</p>\n",
                esc(&self.labels.shown(shown, t.source_rows()))
            ));
        }
        if shown < t.rows.len() {
            self.fidelity.note(
                FidelityKind::RowsCapped {
                    shown,
                    total: t.source_rows(),
                },
                t.title.as_deref().or(t.cell_id.as_deref()),
            );
        }
        if let Some(c) = t.caption.as_deref().filter(|c| !c.is_empty()) {
            self.out
                .push_str(&format!("<p class=\"note\">{}</p>\n", esc(c)));
        }
        self.out.push_str("</figure>\n");
    }

    fn themed_svg(&mut self, light: &svg::Scene, dark: &svg::Scene, id: &str) {
        self.out.push_str(&format!(
            "<div class=\"chart-light\">{}</div>\n",
            to_svg(light, &format!("{id}-l"))
        ));
        self.out.push_str(&format!(
            "<div class=\"chart-dark\">{}</div>\n",
            to_svg(dark, &format!("{id}-d"))
        ));
    }

    fn chart(&mut self, c: &super::doc::ChartBlock) {
        self.figure += 1;
        let id = format!("chart-{}", self.figure);
        let opts = |theme: &'static ReportTheme| self.options.chart_options(theme);
        let light = svg::render_chart(&c.spec, &c.data, opts(&LIGHT));
        let dark = svg::render_chart(&c.spec, &c.data, opts(&DARK));
        self.fidelity
            .note_chart(&light, c.spec.title.as_deref().or(c.cell_id.as_deref()));
        self.fidelity.note_once(FidelityKind::StaticCharts);
        self.out.push_str("<figure class=\"chart\">\n");
        // The incomplete-data badge is drawn inside the chart itself.
        self.themed_svg(&light.scene, &dark.scene, &id);
        if let Some(caption) = c.caption.as_deref() {
            self.out.push_str("<figcaption class=\"note\">");
            self.out.push_str(&esc(caption));
            self.out.push_str("</figcaption>\n");
        }
        self.out.push_str("</figure>\n");
    }

    fn kpi(&mut self, k: &KpiBlock) {
        self.figure += 1;
        let status = k.resolved_status();
        let class = match status {
            Some(super::doc::Status::Good) => " good",
            Some(super::doc::Status::Warning) => " warning",
            Some(super::doc::Status::Critical) => " critical",
            _ => "",
        };
        self.out.push_str(&format!("<div class=\"kpi{class}\">\n"));
        self.out.push_str(&format!(
            "<div class=\"kpi-label\">{}</div>\n",
            esc(&k.label)
        ));
        self.out.push_str(&format!(
            "<div class=\"kpi-value\">{}</div>\n",
            esc(&svg::kpi_value_text(k))
        ));
        if let Some(delta) = svg::kpi_delta_text(k, self.labels) {
            let dir = match k.delta() {
                Some(0.0) => "flat",
                Some(d) if (d > 0.0) == k.higher_is_better => "up",
                _ => "down",
            };
            self.out.push_str(&format!(
                "<div class=\"kpi-delta {dir}\">{}</div>\n",
                esc(&delta)
            ));
        }
        if let Some(s) = status {
            self.out.push_str(&format!(
                "<div class=\"kpi-status{class}\">{}</div>\n",
                esc(self.labels.status(s))
            ));
        }
        if k.sparkline.len() >= 2 {
            let id = format!("spark-{}", self.figure);
            let light = svg::sparkline(&k.sparkline, 220.0, 36.0, &LIGHT);
            let dark = svg::sparkline(&k.sparkline, 220.0, 36.0, &DARK);
            self.themed_svg(&light, &dark, &id);
        }
        if let Some(c) = k.caption.as_deref().filter(|c| !c.is_empty()) {
            self.out
                .push_str(&format!("<p class=\"note\">{}</p>\n", esc(c)));
        }
        self.out.push_str("</div>\n");
    }

    fn callout(&mut self, c: &Callout) {
        let kind = match c.kind {
            super::doc::CalloutKind::Fact => "fact",
            super::doc::CalloutKind::Derived => "derived",
            super::doc::CalloutKind::Hypothesis => "hypothesis",
            super::doc::CalloutKind::Recommendation => "recommendation",
            super::doc::CalloutKind::Warning => "warning",
            super::doc::CalloutKind::Note => "note",
        };
        self.out.push_str(&format!(
            "<aside class=\"callout {kind}\">\n<p class=\"callout-kind\">{}</p>\n",
            esc(self.labels.callout(c.kind))
        ));
        if let Some(t) = c.title.as_deref().filter(|t| !t.is_empty()) {
            self.out
                .push_str(&format!("<p><strong>{}</strong></p>\n", esc(t)));
        }
        let parsed = markdown::parse(&c.text);
        self.fidelity.absorb_markdown(parsed.removed);
        self.md_blocks(&parsed.blocks);
        if !c.evidence.is_empty() {
            self.out.push_str(&format!(
                "<p class=\"note\">{}: {}</p>\n",
                esc(&self.labels.evidence),
                esc(&c.evidence.join(", "))
            ));
        }
        self.out.push_str("</aside>\n");
    }

    fn assertion(&mut self, a: &AssertionResult) {
        let status = format!("{:?}", a.status).to_lowercase();
        self.out.push_str(&format!(
            "<div class=\"assertion\"><span class=\"pill {status}\">{}</span><strong>{}</strong>",
            esc(self.labels.assertion(a.status)),
            esc(&a.name)
        ));
        if let Some(e) = a.expected.as_deref() {
            self.out.push_str(&format!(
                "<span class=\"note\">{}: {}</span>",
                esc(&self.labels.expected),
                esc(e)
            ));
        }
        if let Some(v) = a.actual.as_deref() {
            self.out.push_str(&format!(
                "<span class=\"note\">{}: {}</span>",
                esc(&self.labels.actual),
                esc(v)
            ));
        }
        if let Some(m) = a.message.as_deref().filter(|m| !m.is_empty()) {
            self.out.push_str(&format!("<span>{}</span>", esc(m)));
        }
        self.out.push_str("</div>\n");
    }

    fn code(&mut self, c: &CodeBlock) {
        self.out.push_str("<figure class=\"code\">\n");
        if let Some(cap) = c.caption.as_deref().or(c.cell_id.as_deref()) {
            self.out
                .push_str(&format!("<figcaption>{}</figcaption>\n", esc(cap)));
        }
        let lang: String = c
            .language
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .take(16)
            .collect();
        self.out.push_str(&format!(
            "<pre><code class=\"language-{}\">{}</code></pre>\n</figure>\n",
            esc(&lang),
            esc(&c.source)
        ));
    }

    fn provenance(&mut self, p: &super::doc::Provenance) {
        let lines = plan::provenance_lines(p, self.labels);
        if lines.is_empty() {
            return;
        }
        self.out.push_str(&format!(
            "<dl class=\"provenance\" aria-label=\"{}\">\n",
            esc(&self.labels.provenance)
        ));
        for (label, value) in lines {
            self.out.push_str(&format!(
                "<dt>{}</dt><dd>{}</dd>\n",
                esc(&label),
                esc(&value)
            ));
        }
        self.out.push_str("</dl>\n");
    }

    fn image(&mut self, img: &ImageBlock) {
        let src = match &img.image {
            ImageData::Svg { markup }
                if !svg_is_unsafe(markup) && markup.len() <= super::text::MAX_IMAGE_BYTES =>
            {
                Some(format!(
                    "data:image/svg+xml;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(markup.as_bytes())
                ))
            }
            ImageData::Png { bytes } if png_is_acceptable(bytes) => Some(format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )),
            _ => None,
        };
        self.out.push_str("<figure class=\"image\">\n");
        match src {
            Some(src) => self.out.push_str(&format!(
                "<img alt=\"{}\" src=\"{}\">\n",
                esc(&img.alt),
                src
            )),
            None => {
                self.fidelity
                    .note(FidelityKind::ImageDropped, img.caption.as_deref());
                self.out.push_str(&format!(
                    "<p class=\"note\">{}</p>\n",
                    esc(&self.labels.image_unavailable)
                ));
            }
        }
        if let Some(c) = img.caption.as_deref().filter(|c| !c.is_empty()) {
            self.out.push_str(&format!(
                "<figcaption class=\"note\">{}</figcaption>\n",
                esc(c)
            ));
        }
        self.out.push_str("</figure>\n");
    }

    fn appendix(&mut self) {
        let sql = plan::appendix_sql(self.doc);
        let provenance = plan::appendix_provenance(self.doc);
        if !sql.is_empty() {
            self.out.push_str("<section class=\"appendix\">\n");
            let title = self.labels.appendix_sql.clone();
            self.heading(1, &title);
            for c in sql {
                self.code(c);
            }
            self.out.push_str("</section>\n");
        }
        if !provenance.is_empty() {
            self.out.push_str("<section class=\"appendix\">\n");
            let title = self.labels.appendix_provenance.clone();
            self.heading(1, &title);
            for p in provenance {
                self.provenance(p);
            }
            self.out.push_str("</section>\n");
        }
    }

    fn md_blocks(&mut self, blocks: &[MdBlock]) {
        for block in blocks {
            match block {
                MdBlock::Paragraph(c) => {
                    self.out.push_str("<p>");
                    self.inlines(c);
                    self.out.push_str("</p>\n");
                }
                MdBlock::Heading { level, content } => {
                    let text = markdown::plain(content);
                    self.heading(*level, &text);
                }
                MdBlock::List {
                    ordered,
                    start,
                    items,
                } => {
                    if *ordered {
                        if *start != 1 {
                            self.out.push_str(&format!("<ol start=\"{start}\">\n"));
                        } else {
                            self.out.push_str("<ol>\n");
                        }
                    } else {
                        self.out.push_str("<ul>\n");
                    }
                    for item in items {
                        self.out.push_str("<li>");
                        match item.checked {
                            Some(true) => self.out.push_str("☑ "),
                            Some(false) => self.out.push_str("☐ "),
                            None => {}
                        }
                        if let [MdBlock::Paragraph(c)] = item.blocks.as_slice() {
                            self.inlines(c);
                        } else {
                            self.md_blocks(&item.blocks);
                        }
                        self.out.push_str("</li>\n");
                    }
                    self.out
                        .push_str(if *ordered { "</ol>\n" } else { "</ul>\n" });
                }
                MdBlock::Quote(inner) => {
                    self.out.push_str("<blockquote>\n");
                    self.md_blocks(inner);
                    self.out.push_str("</blockquote>\n");
                }
                MdBlock::Code { language, text } => {
                    let class = language
                        .as_deref()
                        .map(|l| format!(" class=\"language-{}\"", esc(l)))
                        .unwrap_or_default();
                    self.out
                        .push_str(&format!("<pre><code{class}>{}</code></pre>\n", esc(text)));
                }
                MdBlock::Table {
                    align,
                    header,
                    rows,
                } => {
                    let style = |i: usize| match align.get(i) {
                        Some(Align::Right) => " class=\"num\"",
                        Some(Align::Center) => " style=\"text-align:center\"",
                        _ => "",
                    };
                    self.out
                        .push_str("<div class=\"scroll\"><table>\n<thead><tr>");
                    for (i, cell) in header.iter().enumerate() {
                        self.out.push_str(&format!("<th{}>", style(i)));
                        self.inlines(cell);
                        self.out.push_str("</th>");
                    }
                    self.out.push_str("</tr></thead>\n<tbody>\n");
                    for row in rows {
                        self.out.push_str("<tr>");
                        for (i, cell) in row.iter().enumerate() {
                            self.out.push_str(&format!("<td{}>", style(i)));
                            self.inlines(cell);
                            self.out.push_str("</td>");
                        }
                        self.out.push_str("</tr>\n");
                    }
                    self.out.push_str("</tbody>\n</table></div>\n");
                }
                MdBlock::Rule => self.out.push_str("<hr>\n"),
            }
        }
    }

    fn inlines(&mut self, inlines: &[Inline]) {
        for inline in inlines {
            match inline {
                Inline::Text(t) => self.out.push_str(&esc(t)),
                Inline::Strong(c) => {
                    self.out.push_str("<strong>");
                    self.inlines(c);
                    self.out.push_str("</strong>");
                }
                Inline::Emphasis(c) => {
                    self.out.push_str("<em>");
                    self.inlines(c);
                    self.out.push_str("</em>");
                }
                Inline::Strike(c) => {
                    self.out.push_str("<del>");
                    self.inlines(c);
                    self.out.push_str("</del>");
                }
                Inline::Code(t) => self.out.push_str(&format!("<code>{}</code>", esc(t))),
                Inline::Link { url, content } => {
                    let external = !url.starts_with('#');
                    let rel = if external {
                        " rel=\"noopener noreferrer nofollow\""
                    } else {
                        ""
                    };
                    self.out
                        .push_str(&format!("<a href=\"{}\"{rel}>", esc(url)));
                    self.inlines(content);
                    self.out.push_str("</a>");
                }
                Inline::Image { src, alt } => {
                    self.out
                        .push_str(&format!("<img alt=\"{}\" src=\"{}\">", esc(alt), esc(src)));
                }
                Inline::Break => self.out.push_str("<br>"),
            }
        }
    }
}
