//! Flowing the report's blocks onto pages.

use sqlly_core::CellValue;

use super::fonts::FontId;
use super::{BODY, DEFAULT_ROW_CAP, Layout, Op, OutlineEntry, Paint, Pen, Raster, rounded_rect};
use crate::report::doc::{
    AssertionResult, Block, Callout, CodeBlock, ImageBlock, ImageData, KpiBlock, Provenance,
    TableBlock,
};
use crate::report::fidelity::FidelityKind;
use crate::report::format::format_cell;
use crate::report::markdown::{self, Align, Inline, MdBlock};
use crate::report::plan;
use crate::report::svg;
use crate::report::svg::scene::Cmd;
use crate::report::text::{png_is_acceptable, safe_link, svg_is_unsafe};
use crate::report::theme::Rgb;

#[derive(Debug, Clone, PartialEq)]
struct Style {
    font: FontId,
    size: f32,
    color: Rgb,
    bold: bool,
    italic: bool,
    link: Option<String>,
    code: bool,
    strike: bool,
}

#[derive(Debug, Clone)]
struct Span {
    text: String,
    style: Style,
}

#[derive(Debug, Clone)]
struct Piece {
    text: String,
    style: Style,
    x: f32,
    w: f32,
}

struct LineBox {
    pieces: Vec<Piece>,
    height: f32,
}

const LEADING: f32 = 1.38;

impl Layout<'_> {
    fn style(&self, size: f32) -> Style {
        Style {
            font: FontId::Sans,
            size,
            color: self.theme.text,
            bold: false,
            italic: false,
            link: None,
            code: false,
            strike: false,
        }
    }

    fn spans(&self, inlines: &[Inline], style: &Style, out: &mut Vec<Span>) {
        for inline in inlines {
            match inline {
                Inline::Text(t) => out.push(Span {
                    text: t.clone(),
                    style: style.clone(),
                }),
                Inline::Strong(c) => self.spans(
                    c,
                    &Style {
                        bold: true,
                        ..style.clone()
                    },
                    out,
                ),
                Inline::Emphasis(c) => self.spans(
                    c,
                    &Style {
                        italic: true,
                        ..style.clone()
                    },
                    out,
                ),
                Inline::Strike(c) => self.spans(
                    c,
                    &Style {
                        strike: true,
                        ..style.clone()
                    },
                    out,
                ),
                Inline::Code(t) => out.push(Span {
                    text: t.clone(),
                    style: Style {
                        font: FontId::Mono,
                        size: style.size * 0.9,
                        code: true,
                        ..style.clone()
                    },
                }),
                Inline::Link { url, content } => {
                    let link = if url.starts_with('#') {
                        None
                    } else {
                        safe_link(url)
                    };
                    self.spans(
                        content,
                        &Style {
                            color: self.theme.accent,
                            link,
                            ..style.clone()
                        },
                        out,
                    )
                }
                Inline::Image { alt, .. } => out.push(Span {
                    text: alt.clone(),
                    style: Style {
                        italic: true,
                        color: self.theme.text_dim,
                        ..style.clone()
                    },
                }),
                Inline::Break => out.push(Span {
                    text: "\n".into(),
                    style: style.clone(),
                }),
            }
        }
    }

    /// Greedy line breaking of styled text into lines `width` wide.
    fn wrap(&self, spans: &[Span], width: f32) -> Vec<LineBox> {
        let mut lines: Vec<LineBox> = Vec::new();
        let mut pieces: Vec<Piece> = Vec::new();
        let mut x = 0.0f32;
        let base = spans.first().map(|s| s.style.size).unwrap_or(BODY);
        let flush = |pieces: &mut Vec<Piece>, lines: &mut Vec<LineBox>| {
            while pieces.last().is_some_and(|p| p.text.trim().is_empty()) {
                pieces.pop();
            }
            let size = pieces.iter().map(|p| p.style.size).fold(base, f32::max);
            lines.push(LineBox {
                pieces: std::mem::take(pieces),
                height: size * LEADING,
            });
        };
        for span in spans {
            let mut token = String::new();
            let mut tokens: Vec<String> = Vec::new();
            for c in span.text.chars() {
                if c == '\n' {
                    if !token.is_empty() {
                        tokens.push(std::mem::take(&mut token));
                    }
                    tokens.push("\n".into());
                } else if c.is_whitespace() {
                    if !token.is_empty() && !token.ends_with(' ') {
                        tokens.push(std::mem::take(&mut token));
                    }
                    if token.is_empty() {
                        token.push(' ');
                        tokens.push(std::mem::take(&mut token));
                    }
                } else {
                    token.push(c);
                }
            }
            if !token.is_empty() {
                tokens.push(token);
            }
            for token in tokens {
                if token == "\n" {
                    flush(&mut pieces, &mut lines);
                    x = 0.0;
                    continue;
                }
                let st = &span.style;
                if token == " " {
                    if x > 0.0 {
                        let w = self.measure(" ", st.font, st.size);
                        push_piece(&mut pieces, " ", st, x, w);
                        x += w;
                    }
                    continue;
                }
                let w = self.measure(&token, st.font, st.size);
                if x + w > width && x > 0.0 {
                    flush(&mut pieces, &mut lines);
                    x = 0.0;
                }
                if w > width {
                    let mut chunk = String::new();
                    let mut cw = 0.0;
                    for c in token.chars() {
                        let w = self.measure(c.encode_utf8(&mut [0; 4]), st.font, st.size);
                        if x + cw + w > width && !chunk.is_empty() {
                            push_piece(&mut pieces, &chunk, st, x, cw);
                            flush(&mut pieces, &mut lines);
                            x = 0.0;
                            chunk.clear();
                            cw = 0.0;
                        }
                        chunk.push(c);
                        cw += w;
                    }
                    push_piece(&mut pieces, &chunk, st, x, cw);
                    x += cw;
                } else {
                    push_piece(&mut pieces, &token, st, x, w);
                    x += w;
                }
            }
        }
        if !pieces.is_empty() || lines.is_empty() {
            flush(&mut pieces, &mut lines);
        }
        lines
    }

    fn draw_lines(&mut self, lines: Vec<LineBox>, x: f32, bar: Option<(f32, Rgb)>) {
        for line in lines {
            self.ensure(line.height);
            let baseline = self.y + line.height * 0.74;
            if let Some((bx, color)) = bar {
                self.push(Op::Rect {
                    x: bx,
                    y: self.y,
                    w: 3.0,
                    h: line.height,
                    fill: Some(Paint { color, alpha: 1.0 }),
                    stroke: None,
                });
            }
            for p in line.pieces {
                let px = x + p.x;
                if p.style.code {
                    let pad = 1.5;
                    let surface = self.theme.surface;
                    self.push(Op::Rect {
                        x: px - pad,
                        y: baseline - p.style.size * 0.95,
                        w: p.w + 2.0 * pad,
                        h: p.style.size * 1.25,
                        fill: Some(Paint {
                            color: surface,
                            alpha: 1.0,
                        }),
                        stroke: None,
                    });
                }
                self.text(
                    px,
                    baseline,
                    &p.text,
                    p.style.font,
                    p.style.size,
                    p.style.color,
                    p.style.bold,
                    p.style.italic,
                );
                if p.style.strike || p.style.link.is_some() {
                    let y = if p.style.strike {
                        baseline - p.style.size * 0.3
                    } else {
                        baseline + p.style.size * 0.12
                    };
                    self.push(Op::Path {
                        cmds: vec![Cmd::Move(px, y), Cmd::Line(px + p.w, y)],
                        fill: None,
                        stroke: Some(Pen::solid(p.style.color, p.style.size * 0.06)),
                    });
                }
                if let Some(url) = p.style.link.clone() {
                    self.link([px, baseline - p.style.size, p.w, p.style.size * 1.3], url);
                }
            }
            self.y += line.height;
        }
    }

    fn paragraph(&mut self, text: &str, size: f32, color: Rgb, bold: bool, x: f32, width: f32) {
        let style = Style {
            color,
            bold,
            ..self.style(size)
        };
        let lines = self.wrap(
            &[Span {
                text: text.to_string(),
                style,
            }],
            width,
        );
        self.draw_lines(lines, x, None);
    }

    pub fn cover(&mut self) {
        let doc = self.doc;
        let full = doc.meta.template.is_some() && doc.meta.layout.cover;
        if full {
            self.y = self.top + 140.0;
        }
        let title_size = if full { 26.0 } else { 21.0 };
        let (left, w) = (self.left, self.width());
        let text = self.theme.text;
        let dim = self.theme.text_dim;
        self.paragraph(&doc.meta.title.clone(), title_size, text, true, left, w);
        if let Some(s) = doc.meta.subtitle.clone().filter(|s| !s.is_empty()) {
            self.y += 2.0;
            self.paragraph(&s, if full { 15.0 } else { 12.5 }, dim, false, left, w);
        }
        if doc.meta.layout.cover {
            let lines = plan::meta_lines(doc, self.labels);
            if !lines.is_empty() {
                self.y += 10.0;
                let label_w = lines
                    .iter()
                    .map(|(l, _)| self.measure(l, FontId::Sans, 9.0))
                    .fold(0.0, f32::max)
                    .min(w * 0.3)
                    + 10.0;
                for (label, value) in lines {
                    let start = self.y;
                    self.paragraph(&label, 9.0, dim, true, left, label_w - 6.0);
                    let after_label = self.y;
                    self.y = start;
                    self.paragraph(&value, 9.0, text, false, left + label_w, w - label_w);
                    self.y = self.y.max(after_label);
                }
            }
        }
        self.y += 8.0;
        let border = self.theme.border;
        self.rule(border);
        self.y += 14.0;
        if full {
            self.new_page();
            self.cover_pages = 1;
        }
    }

    pub fn heading(&mut self, level: u8, text: &str) {
        let size = match level {
            1 => 16.5,
            2 => 13.5,
            3 => 12.0,
            _ => 11.0,
        };
        self.gap(if level <= 2 { 12.0 } else { 8.0 });
        self.ensure(size * LEADING + 48.0);
        self.outline.push(OutlineEntry {
            level,
            title: text.to_string(),
            page: self.page_index(),
            y: self.y,
        });
        let (left, w, color) = (self.left, self.width(), self.theme.text);
        self.paragraph(text, size, color, true, left, w);
        self.y += 3.0;
    }

    pub fn block(&mut self, block: &Block) {
        if !plan::inline(block, self.doc) {
            return;
        }
        match block {
            Block::Heading(h) => self.heading(h.level(), &h.text),
            Block::Markdown(m) => {
                let parsed = markdown::parse(&m.source);
                self.fidelity.absorb_markdown(parsed.removed);
                let (left, w) = (self.left, self.width());
                self.md_blocks(&parsed.blocks, left, w, None);
            }
            Block::Table(t) => self.data_table(t, t.title.as_deref()),
            Block::Chart(c) => self.chart(c),
            Block::Kpi(k) => {
                let w = self.width();
                self.kpi_row(&[(k, self.left, w)]);
            }
            Block::Callout(c) => self.callout(c),
            Block::Assertion(a) => self.assertion(a),
            Block::Code(c) => self.code(c),
            Block::Provenance(p) => self.provenance(p),
            Block::PageBreak => {
                if !self.at_top() {
                    self.new_page();
                }
            }
            Block::Section(s) => {
                if let Some(t) = s.title.as_deref().filter(|t| !t.is_empty()) {
                    self.heading(1, t);
                }
                for b in &s.blocks {
                    self.block(b);
                }
            }
            Block::Columns(cols) => {
                let all_kpis = cols.columns.iter().all(|c| {
                    !c.blocks.is_empty() && c.blocks.iter().all(|b| matches!(b, Block::Kpi(_)))
                });
                if all_kpis && cols.columns.len() > 1 {
                    self.kpi_columns(cols);
                } else {
                    if cols.columns.len() > 1 {
                        self.fidelity.note_once(FidelityKind::ColumnsStacked);
                    }
                    for col in &cols.columns {
                        for b in &col.blocks {
                            self.block(b);
                        }
                    }
                }
            }
            Block::Image(img) => self.image(img),
        }
    }

    fn md_blocks(&mut self, blocks: &[MdBlock], x: f32, width: f32, bar: Option<(f32, Rgb)>) {
        for block in blocks {
            match block {
                MdBlock::Paragraph(content) => {
                    let mut spans = Vec::new();
                    let color = if bar.is_some() {
                        self.theme.text_dim
                    } else {
                        self.theme.text
                    };
                    let style = Style {
                        color,
                        ..self.style(BODY)
                    };
                    self.spans(content, &style, &mut spans);
                    let lines = self.wrap(&spans, width);
                    self.draw_lines(lines, x, bar);
                    self.y += 5.0;
                }
                MdBlock::Heading { level, content } => {
                    self.heading(*level, &markdown::plain(content))
                }
                MdBlock::List {
                    ordered,
                    start,
                    items,
                } => {
                    for (i, item) in items.iter().enumerate() {
                        let marker = match (item.checked, ordered) {
                            (Some(true), _) => "☑".to_string(),
                            (Some(false), _) => "☐".to_string(),
                            (None, true) => format!("{}.", *start as usize + i),
                            (None, false) => "•".to_string(),
                        };
                        self.ensure(BODY * LEADING);
                        let baseline = self.y + BODY * LEADING * 0.74;
                        let color = self.theme.text_dim;
                        self.text(
                            x,
                            baseline,
                            &marker,
                            FontId::Sans,
                            BODY,
                            color,
                            false,
                            false,
                        );
                        let indent = 16.0;
                        let saved = self.y;
                        self.md_blocks(&item.blocks, x + indent, width - indent, bar);
                        if (self.y - saved).abs() < 0.1 {
                            self.y += BODY * LEADING;
                        }
                        self.y -= 3.0;
                    }
                    self.y += 4.0;
                }
                MdBlock::Quote(inner) => {
                    let color = self.theme.border;
                    self.md_blocks(inner, x + 12.0, width - 12.0, Some((x, color)));
                }
                MdBlock::Code { text, .. } => self.code_text(text, x, width),
                MdBlock::Table {
                    align,
                    header,
                    rows,
                } => {
                    let header: Vec<String> = header.iter().map(|c| markdown::plain(c)).collect();
                    let right: Vec<bool> = (0..header.len())
                        .map(|i| align.get(i) == Some(&Align::Right))
                        .collect();
                    let rows: Vec<Vec<String>> = rows
                        .iter()
                        .map(|r| r.iter().map(|c| markdown::plain(c)).collect())
                        .collect();
                    self.grid(&header, &right, &rows, None);
                }
                MdBlock::Rule => {
                    self.y += 4.0;
                    let border = self.theme.border;
                    self.rule(border);
                    self.y += 8.0;
                }
            }
        }
    }

    /// A table that flows across pages with its header repeated.
    fn grid(
        &mut self,
        header: &[String],
        right: &[bool],
        rows: &[Vec<String>],
        block: Option<&str>,
    ) {
        let size = 8.0;
        let pad = 4.0;
        let lh = size * 1.3;
        let width = self.width();
        let sample = rows.len().min(200);
        let mut natural: Vec<f32> = header
            .iter()
            .map(|h| self.measure(h, FontId::Sans, size) * 1.04 + 2.0 * pad)
            .collect();
        for row in &rows[..sample] {
            for (i, cell) in row.iter().enumerate().take(natural.len()) {
                let w = self.measure(cell, FontId::Sans, size) + 2.0 * pad;
                natural[i] = natural[i].max(w.min(width * 0.6));
            }
        }
        let min_of = |n: f32| n.clamp(26.0, 44.0);
        let mut count = natural.len();
        while count > 1 && natural[..count].iter().map(|n| min_of(*n)).sum::<f32>() > width {
            count -= 1;
        }
        if count < natural.len() {
            self.fidelity.note(
                FidelityKind::ColumnsOmitted {
                    shown: count,
                    total: natural.len(),
                },
                block,
            );
        }
        let natural = &natural[..count];
        let total: f32 = natural.iter().sum();
        let widths: Vec<f32> = if total <= width {
            let extra = (width - total) / count.max(1) as f32;
            natural.iter().map(|n| n + extra.min(40.0)).collect()
        } else {
            let mins: Vec<f32> = natural.iter().map(|n| min_of(*n)).collect();
            let spare = width - mins.iter().sum::<f32>();
            let want: f32 = natural.iter().zip(&mins).map(|(n, m)| n - m).sum();
            natural
                .iter()
                .zip(&mins)
                .map(|(n, m)| {
                    m + if want > 0.0 {
                        (n - m) / want * spare
                    } else {
                        0.0
                    }
                })
                .collect()
        };
        let lines_of = |this: &Self, text: &str, w: f32, bold: bool| -> (Vec<String>, bool) {
            let style = Style {
                bold,
                ..this.style(size)
            };
            let wrapped = this.wrap(
                &[Span {
                    text: text.to_string(),
                    style,
                }],
                (w - 2.0 * pad).max(4.0),
            );
            let mut out: Vec<String> = wrapped
                .iter()
                .map(|l| l.pieces.iter().map(|p| p.text.as_str()).collect::<String>())
                .collect();
            let cut = out.len() > 3;
            if cut {
                out.truncate(3);
                let last = out[2].clone() + "…";
                out[2] = this.fit(&last, FontId::Sans, size, w - 2.0 * pad);
            }
            (out, cut)
        };
        let theme = self.theme;
        let draw_row =
            |this: &mut Self, cells: &[Vec<String>], height: f32, head: bool, stripe: bool| {
                let y = this.y;
                if head || stripe {
                    this.push(Op::Rect {
                        x: this.left,
                        y,
                        w: widths.iter().sum(),
                        h: height,
                        fill: Some(Paint {
                            color: theme.surface,
                            alpha: 1.0,
                        }),
                        stroke: None,
                    });
                }
                let mut x = this.left;
                for (i, lines) in cells.iter().enumerate() {
                    let w = widths[i];
                    for (k, line) in lines.iter().enumerate() {
                        let baseline = y + pad + lh * (k as f32) + size * 0.95;
                        let tw = this.measure(line, FontId::Sans, size);
                        let tx = if right.get(i).copied().unwrap_or(false) {
                            x + w - pad - tw
                        } else {
                            x + pad
                        };
                        this.text(
                            tx,
                            baseline,
                            line,
                            FontId::Sans,
                            size,
                            theme.text,
                            head,
                            false,
                        );
                    }
                    x += w;
                }
                let bottom = y + height;
                this.push(Op::Path {
                    cmds: vec![
                        Cmd::Move(this.left, bottom),
                        Cmd::Line(this.left + widths.iter().sum::<f32>(), bottom),
                    ],
                    fill: None,
                    stroke: Some(Pen::solid(
                        if head { theme.border } else { theme.grid },
                        if head { 0.8 } else { 0.5 },
                    )),
                });
                this.y = bottom;
            };
        let mut header_cells = Vec::new();
        let mut header_h: f32 = 0.0;
        for (i, h) in header.iter().take(count).enumerate() {
            let (lines, _) = lines_of(self, h, widths[i], true);
            header_h = header_h.max(lines.len() as f32 * lh + 2.0 * pad);
            header_cells.push(lines);
        }
        self.ensure(header_h + lh * 2.0 + 2.0 * pad);
        draw_row(self, &header_cells, header_h, true, false);
        let mut truncated = false;
        for (r, row) in rows.iter().enumerate() {
            let mut cells = Vec::with_capacity(count);
            let mut h: f32 = 0.0;
            for (i, w) in widths.iter().enumerate() {
                let text = row.get(i).map(String::as_str).unwrap_or("");
                let (lines, cut) = lines_of(self, text, *w, false);
                truncated |= cut;
                h = h.max(lines.len().max(1) as f32 * lh + 2.0 * pad);
                cells.push(lines);
            }
            if self.y + h > self.bottom {
                self.new_page();
                draw_row(self, &header_cells, header_h, true, false);
            }
            draw_row(self, &cells, h, false, r % 2 == 1);
        }
        if truncated {
            self.fidelity.note_once(FidelityKind::CellTextTruncated);
        }
        self.y += 6.0;
    }

    fn note_line(&mut self, text: &str) {
        let (left, w, dim) = (self.left, self.width(), self.theme.text_dim);
        self.paragraph(text, 8.0, dim, false, left, w);
    }

    fn data_table(&mut self, t: &TableBlock, title: Option<&str>) {
        let badge = self.labels.completeness(t.completeness);
        self.gap(6.0);
        if title.is_some() || badge.is_some() {
            self.ensure(40.0);
            let size = 10.5;
            let baseline = self.y + size;
            let mut x = self.left;
            if let Some(title) = title {
                let color = self.theme.text;
                x += self.text(x, baseline, title, FontId::Sans, size, color, true, false) + 8.0;
            }
            if let Some(b) = &badge {
                let color = self.theme.warning;
                self.text(x, baseline, b, FontId::Sans, 8.5, color, true, false);
            }
            self.y += size * 1.6;
        }
        let kinds: Vec<_> = t.columns.iter().map(|c| c.kind()).collect();
        let header: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
        let right: Vec<bool> = kinds.iter().map(|k| k.is_right_aligned()).collect();
        let mut shown = t.shown_rows(self.doc.meta.layout.table_row_cap);
        if t.max_rows.is_none() && self.doc.meta.layout.table_row_cap.is_none() {
            shown = shown.min(DEFAULT_ROW_CAP);
        }
        let rows: Vec<Vec<String>> = t.rows[..shown]
            .iter()
            .map(|row| {
                t.columns
                    .iter()
                    .zip(&kinds)
                    .enumerate()
                    .map(|(i, (c, k))| {
                        let v = row.get(i).unwrap_or(&CellValue::Null);
                        crate::report::text::one_line(&format_cell(v, *k, &c.format))
                    })
                    .collect()
            })
            .collect();
        let name = title.or(t.cell_id.as_deref());
        self.grid(&header, &right, &rows, name);
        if t.data_withheld {
            let text = self.labels.data_withheld.clone();
            self.note_line(&text);
        } else if t.rows.is_empty() {
            let text = self.labels.no_rows.clone();
            self.note_line(&text);
        } else if shown < t.rows.len() || t.source_rows() > shown as u64 {
            let text = self.labels.shown(shown, t.source_rows());
            self.note_line(&text);
        }
        if shown < t.rows.len() {
            self.fidelity.note(
                FidelityKind::RowsCapped {
                    shown,
                    total: t.source_rows(),
                },
                name,
            );
        }
        if let Some(c) = t.caption.as_deref().filter(|c| !c.is_empty()) {
            self.note_line(c);
        }
        self.y += 4.0;
    }

    fn chart(&mut self, c: &crate::report::doc::ChartBlock) {
        let rendered = svg::render_chart(&c.spec, &c.data, self.options.chart_options(self.theme));
        self.fidelity
            .note_chart(&rendered, c.spec.title.as_deref().or(c.cell_id.as_deref()));
        self.fidelity.note_once(FidelityKind::StaticCharts);
        let scene = rendered.scene;
        let mut s = self.width() / scene.width;
        let max_h = self.bottom - self.top - 40.0;
        if scene.height * s > max_h {
            s = max_h / scene.height;
        }
        self.gap(6.0);
        self.ensure(scene.height * s + 16.0);
        let x = self.left + (self.width() - scene.width * s) / 2.0;
        let y = self.y;
        self.paint(&scene, x, y, s);
        self.y += scene.height * s + 4.0;
        if let Some(cap) = c.caption.clone().filter(|c| !c.is_empty()) {
            self.note_line(&cap);
        }
        self.y += 6.0;
    }

    fn kpi_row(&mut self, cards: &[(&KpiBlock, f32, f32)]) {
        let h = 92.0;
        self.gap(4.0);
        self.ensure(h + 8.0);
        let y = self.y;
        for (kpi, x, w) in cards {
            let opts = svg::ChartOptions::new(self.theme, self.labels).size(*w, h);
            let scene = svg::kpi_card(kpi, opts);
            self.paint(&scene, *x, y, 1.0);
        }
        self.y = y + h + 8.0;
    }

    fn kpi_columns(&mut self, cols: &crate::report::doc::Columns) {
        let gap = 10.0;
        let total: f32 = cols
            .columns
            .iter()
            .map(|c| f32::from(c.span()))
            .sum::<f32>()
            .max(12.0);
        let unit = (self.width() + gap) / total;
        let rows = cols
            .columns
            .iter()
            .map(|c| c.blocks.len())
            .max()
            .unwrap_or(0);
        for r in 0..rows {
            let mut x = self.left;
            let mut cards = Vec::new();
            for col in &cols.columns {
                let w = unit * f32::from(col.span()) - gap;
                if let Some(Block::Kpi(k)) = col.blocks.get(r) {
                    cards.push((k, x, w));
                }
                x += w + gap;
            }
            self.kpi_row(&cards);
        }
    }

    fn callout(&mut self, c: &Callout) {
        let color = self.theme.callout_color(c.kind);
        let x = self.left + 10.0;
        let w = self.width() - 10.0;
        let bar = Some((self.left, color));
        self.gap(6.0);
        self.ensure(40.0);
        let label = self.labels.callout(c.kind).to_uppercase();
        let lines = self.wrap(
            &[Span {
                text: label,
                style: Style {
                    color,
                    bold: true,
                    ..self.style(8.0)
                },
            }],
            w,
        );
        self.draw_lines(lines, x, bar);
        if let Some(t) = c.title.as_deref().filter(|t| !t.is_empty()) {
            let lines = self.wrap(
                &[Span {
                    text: t.to_string(),
                    style: Style {
                        bold: true,
                        ..self.style(BODY)
                    },
                }],
                w,
            );
            self.draw_lines(lines, x, bar);
        }
        let parsed = markdown::parse(&c.text);
        self.fidelity.absorb_markdown(parsed.removed);
        for block in &parsed.blocks {
            let mut spans = Vec::new();
            match block {
                MdBlock::Paragraph(content) | MdBlock::Heading { content, .. } => {
                    self.spans(content, &self.style(BODY), &mut spans)
                }
                MdBlock::List { items, .. } => {
                    for item in items {
                        spans.push(Span {
                            text: "• ".into(),
                            style: self.style(BODY),
                        });
                        spans.push(Span {
                            text: markdown::plain_blocks(&item.blocks),
                            style: self.style(BODY),
                        });
                        spans.push(Span {
                            text: "\n".into(),
                            style: self.style(BODY),
                        });
                    }
                }
                other => spans.push(Span {
                    text: markdown::plain_blocks(std::slice::from_ref(other)),
                    style: self.style(BODY),
                }),
            }
            let lines = self.wrap(&spans, w);
            self.draw_lines(lines, x, bar);
        }
        if !c.evidence.is_empty() {
            let text = format!("{}: {}", self.labels.evidence, c.evidence.join(", "));
            let lines = self.wrap(
                &[Span {
                    text,
                    style: Style {
                        color: self.theme.text_dim,
                        ..self.style(8.0)
                    },
                }],
                w,
            );
            self.draw_lines(lines, x, bar);
        }
        self.y += 8.0;
    }

    fn assertion(&mut self, a: &AssertionResult) {
        let size = 9.0;
        let color = self.theme.assertion_color(a.status);
        let label = self.labels.assertion(a.status).to_string();
        let pill_w = self.measure(&label, FontId::Sans, 8.0) + 12.0;
        self.ensure(size * LEADING * 2.0 + 6.0);
        let y = self.y + 2.0;
        self.push(Op::Path {
            cmds: rounded_rect(self.left, y, pill_w, 13.0, 6.5),
            fill: Some(Paint { color, alpha: 1.0 }),
            stroke: None,
        });
        let ink = self.theme.ink_on(color);
        self.text(
            self.left + 6.0,
            y + 9.5,
            &label,
            FontId::Sans,
            8.0,
            ink,
            true,
            false,
        );
        let x = self.left + pill_w + 8.0;
        let w = self.width() - pill_w - 8.0;
        let lines = self.wrap(
            &[Span {
                text: a.name.clone(),
                style: Style {
                    bold: true,
                    ..self.style(size)
                },
            }],
            w,
        );
        self.draw_lines(lines, x, None);
        let mut details = Vec::new();
        if let Some(e) = a.expected.as_deref() {
            details.push(format!("{}: {e}", self.labels.expected));
        }
        if let Some(v) = a.actual.as_deref() {
            details.push(format!("{}: {v}", self.labels.actual));
        }
        if let Some(m) = a.message.as_deref().filter(|m| !m.is_empty()) {
            details.push(m.to_string());
        }
        if !details.is_empty() {
            let dim = self.theme.text_dim;
            self.paragraph(&details.join(" · "), 8.5, dim, false, x, w);
        }
        self.y += 6.0;
    }

    fn code_text(&mut self, text: &str, x: f32, width: f32) {
        let size = 8.0;
        let lh = size * 1.35;
        let max_chars = ((width - 12.0) / self.measure("M", FontId::Mono, size))
            .floor()
            .max(8.0) as usize;
        self.gap(2.0);
        let surface = self.theme.surface;
        let ink = self.theme.text;
        let mut first = true;
        for raw in text.trim_end_matches('\n').split('\n') {
            let raw = raw.replace('\t', "    ");
            let chars: Vec<char> = raw.chars().collect();
            let chunks: Vec<String> = if chars.is_empty() {
                vec![String::new()]
            } else {
                chars
                    .chunks(max_chars)
                    .map(|c| c.iter().collect())
                    .collect()
            };
            for chunk in chunks {
                let pad_top = if first { 5.0 } else { 0.0 };
                self.ensure(lh + pad_top);
                let fresh = self.at_top();
                let top_pad = if first || fresh { 5.0 } else { 0.0 };
                self.push(Op::Rect {
                    x,
                    y: self.y,
                    w: width,
                    h: lh + top_pad,
                    fill: Some(Paint {
                        color: surface,
                        alpha: 1.0,
                    }),
                    stroke: None,
                });
                self.y += top_pad;
                self.text(
                    x + 6.0,
                    self.y + size * 1.0,
                    &chunk,
                    FontId::Mono,
                    size,
                    ink,
                    false,
                    false,
                );
                self.y += lh;
                first = false;
            }
        }
        self.push(Op::Rect {
            x,
            y: self.y,
            w: width,
            h: 5.0,
            fill: Some(Paint {
                color: surface,
                alpha: 1.0,
            }),
            stroke: None,
        });
        self.y += 11.0;
    }

    pub fn code(&mut self, c: &CodeBlock) {
        if let Some(cap) = c.caption.as_deref().or(c.cell_id.as_deref()) {
            self.gap(4.0);
            self.ensure(30.0);
            let (left, w, dim) = (self.left, self.width(), self.theme.text_dim);
            self.paragraph(cap, 8.5, dim, true, left, w);
        }
        let (left, w) = (self.left, self.width());
        self.code_text(&c.source, left, w);
    }

    pub fn provenance(&mut self, p: &Provenance) {
        let lines = plan::provenance_lines(p, self.labels);
        if lines.is_empty() {
            return;
        }
        self.gap(3.0);
        let text = lines
            .iter()
            .map(|(l, v)| format!("{l}: {v}"))
            .collect::<Vec<_>>()
            .join(" · ");
        let (left, w, dim) = (self.left, self.width(), self.theme.text_dim);
        self.paragraph(&text, 7.5, dim, false, left, w);
        self.y += 4.0;
    }

    fn image(&mut self, img: &ImageBlock) {
        let raster = match &img.image {
            ImageData::Png { bytes } if png_is_acceptable(bytes) => decode(bytes),
            ImageData::Svg { markup } if !svg_is_unsafe(markup) => {
                crate::report::png::rasterize(markup, 2.0)
                    .ok()
                    .and_then(|png| {
                        decode(&png).map(|mut r| {
                            r.width = r.width.max(1);
                            r
                        })
                    })
            }
            _ => None,
        };
        let svg_scale = matches!(img.image, ImageData::Svg { .. });
        let Some(raster) = raster else {
            self.fidelity
                .note(FidelityKind::ImageDropped, img.caption.as_deref());
            let text = self.labels.image_unavailable.clone();
            self.note_line(&text);
            return;
        };
        let px_to_pt = if svg_scale { 0.375 } else { 0.75 };
        let mut w = raster.width as f32 * px_to_pt;
        let mut h = raster.height as f32 * px_to_pt;
        let max_w = self.width();
        let max_h = self.bottom - self.top - 30.0;
        let s = (max_w / w).min(max_h / h).min(1.0);
        w *= s;
        h *= s;
        self.gap(6.0);
        self.ensure(h + 6.0);
        let x = self.left + (self.width() - w) / 2.0;
        self.images.push(raster);
        let index = self.images.len() - 1;
        self.push(Op::Image {
            x,
            y: self.y,
            w,
            h,
            index,
        });
        self.y += h + 4.0;
        if let Some(c) = img.caption.as_deref().filter(|c| !c.is_empty()) {
            self.note_line(c);
        }
        self.y += 6.0;
    }

    pub fn appendix(&mut self) {
        let sql = plan::appendix_sql(self.doc);
        if !sql.is_empty() {
            if !self.at_top() {
                self.new_page();
            }
            let title = self.labels.appendix_sql.clone();
            self.heading(1, &title);
            for c in sql {
                self.code(c);
            }
        }
        let provenance = plan::appendix_provenance(self.doc);
        if !provenance.is_empty() {
            let title = self.labels.appendix_provenance.clone();
            self.heading(1, &title);
            for p in provenance {
                self.provenance(p);
            }
        }
    }
}

fn push_piece(pieces: &mut Vec<Piece>, text: &str, style: &Style, x: f32, w: f32) {
    if let Some(last) = pieces.last_mut()
        && last.style == *style
        && (last.x + last.w - x).abs() < 0.01
    {
        last.text.push_str(text);
        last.w += w;
        return;
    }
    pieces.push(Piece {
        text: text.to_string(),
        style: style.clone(),
        x,
        w,
    });
}

/// A PNG as straight (not premultiplied) RGB plus an alpha plane.
fn decode(bytes: &[u8]) -> Option<Raster> {
    let pixmap = resvg::tiny_skia::Pixmap::decode_png(bytes).ok()?;
    let (w, h) = (pixmap.width(), pixmap.height());
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    let mut alpha = Vec::with_capacity((w * h) as usize);
    let mut opaque = true;
    for px in pixmap.pixels() {
        let c = px.demultiply();
        rgb.extend_from_slice(&[c.red(), c.green(), c.blue()]);
        alpha.push(c.alpha());
        opaque &= c.alpha() == 255;
    }
    Some(Raster {
        width: w,
        height: h,
        rgb,
        alpha: (!opaque).then_some(alpha),
    })
}
