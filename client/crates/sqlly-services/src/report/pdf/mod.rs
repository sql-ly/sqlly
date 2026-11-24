//! Vector PDF reports: embedded subset fonts, paged tables with repeated
//! headers, running headers and footers with page numbers, bookmarks from
//! headings, charts painted as vector paths, link annotations for safe
//! URLs only. Built with `pdf-writer`; deterministic (fixed object order,
//! no clock reads, subset tags derived from the glyph set).

mod blocks;
pub mod fonts;
mod write;

use pdf_writer::Ref;

use super::doc::ReportDoc;
use super::fidelity::{FidelityKind, FidelityReport};
use super::labels::ReportLabels;
use super::svg::scene::{Cmd, Item, Scene};
use super::theme::{ReportTheme, Rgb};
use super::{PageSize, RenderOptions, ReportFormat};
use fonts::{FontId, Fonts, Run};

/// Rows a PDF table prints when nothing else caps it.
pub const DEFAULT_ROW_CAP: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Paint {
    pub color: Rgb,
    pub alpha: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Pen {
    pub color: Rgb,
    pub width: f32,
    pub alpha: f32,
    pub dash: Option<(f32, f32)>,
}

impl Pen {
    pub fn solid(color: Rgb, width: f32) -> Self {
        Self {
            color,
            width,
            alpha: 1.0,
            dash: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TextOp {
    pub x: f32,
    /// Baseline, measured down from the page top.
    pub y: f32,
    pub size: f32,
    pub runs: Vec<Run>,
    pub color: Rgb,
    pub alpha: f32,
    pub bold: bool,
    pub italic: bool,
    /// Degrees clockwise.
    pub rotate: f32,
}

/// A drawing operation in page points, y measured down from the top.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Op {
    Text(TextOp),
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        fill: Option<Paint>,
        stroke: Option<Pen>,
    },
    Path {
        cmds: Vec<Cmd>,
        fill: Option<Paint>,
        stroke: Option<Pen>,
    },
    Image {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        index: usize,
    },
}

#[derive(Debug, Default)]
pub(crate) struct PageOps {
    pub ops: Vec<Op>,
    /// `(x, y, w, h)` in top-down points, and the target URL.
    pub links: Vec<([f32; 4], String)>,
}

pub(crate) struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    pub alpha: Option<Vec<u8>>,
}

pub(crate) struct OutlineEntry {
    pub level: u8,
    pub title: String,
    pub page: usize,
    pub y: f32,
}

pub(crate) struct Layout<'a> {
    pub doc: &'a ReportDoc,
    pub options: &'a RenderOptions,
    pub labels: &'a ReportLabels,
    pub theme: &'static ReportTheme,
    pub fonts: Fonts,
    pub page_w: f32,
    pub page_h: f32,
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
    pub pages: Vec<PageOps>,
    pub y: f32,
    pub outline: Vec<OutlineEntry>,
    pub images: Vec<Raster>,
    pub fidelity: FidelityReport,
    /// Leading pages without the running header (the cover).
    pub cover_pages: usize,
}

pub(crate) const BODY: f32 = 10.0;

impl<'a> Layout<'a> {
    fn new(doc: &'a ReportDoc, options: &'a RenderOptions) -> Result<Self, String> {
        let (mut w, mut h) = match options.page {
            PageSize::A4 => (595.28, 841.89),
            PageSize::Letter => (612.0, 792.0),
        };
        if options.landscape {
            std::mem::swap(&mut w, &mut h);
        }
        let margin = 54.0;
        let mut layout = Self {
            doc,
            options,
            labels: &options.labels,
            theme: ReportTheme::for_mode(options.theme),
            fonts: Fonts::new()?,
            page_w: w,
            page_h: h,
            left: margin,
            right: w - margin,
            top: margin + 6.0,
            bottom: h - margin,
            pages: Vec::new(),
            y: 0.0,
            outline: Vec::new(),
            images: Vec::new(),
            fidelity: FidelityReport::new(ReportFormat::Pdf),
            cover_pages: 0,
        };
        layout.new_page();
        Ok(layout)
    }

    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    pub fn new_page(&mut self) {
        let mut page = PageOps::default();
        if self.theme.mode == super::theme::ThemeMode::Dark {
            page.ops.push(Op::Rect {
                x: 0.0,
                y: 0.0,
                w: self.page_w,
                h: self.page_h,
                fill: Some(Paint {
                    color: self.theme.background,
                    alpha: 1.0,
                }),
                stroke: None,
            });
        }
        self.pages.push(page);
        self.y = self.top;
    }

    pub fn at_top(&self) -> bool {
        self.y <= self.top + 0.5
    }

    /// Starts a new page unless `h` more points fit on this one.
    pub fn ensure(&mut self, h: f32) {
        if self.y + h > self.bottom && !self.at_top() {
            self.new_page();
        }
    }

    pub fn gap(&mut self, h: f32) {
        if !self.at_top() {
            self.y += h;
        }
    }

    pub fn push(&mut self, op: Op) {
        if let Some(page) = self.pages.last_mut() {
            page.ops.push(op);
        }
    }

    pub fn link(&mut self, rect: [f32; 4], url: String) {
        if let Some(page) = self.pages.last_mut() {
            page.links.push((rect, url));
        }
    }

    pub fn page_index(&self) -> usize {
        self.pages.len().saturating_sub(1)
    }

    /// Draws `text` with its baseline at (`x`, `y`); returns its width.
    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &mut self,
        x: f32,
        y: f32,
        text: &str,
        font: FontId,
        size: f32,
        color: Rgb,
        bold: bool,
        italic: bool,
    ) -> f32 {
        let (runs, width) = self.fonts.shape(font, text, size);
        if !runs.is_empty() {
            self.push(Op::Text(TextOp {
                x,
                y,
                size,
                runs,
                color,
                alpha: 1.0,
                bold,
                italic,
                rotate: 0.0,
            }));
        }
        width
    }

    pub fn measure(&self, text: &str, font: FontId, size: f32) -> f32 {
        self.fonts.width(font, text, size)
    }

    /// `text` cut with an ellipsis to fit `max` points.
    pub fn fit(&self, text: &str, font: FontId, size: f32, max: f32) -> String {
        if self.measure(text, font, size) <= max {
            return text.to_string();
        }
        let mut out = String::new();
        let ellipsis = self.measure("…", font, size);
        let mut w = 0.0;
        for c in text.chars() {
            let cw = self.measure(c.encode_utf8(&mut [0; 4]), font, size);
            if w + cw + ellipsis > max {
                break;
            }
            w += cw;
            out.push(c);
        }
        out.push('…');
        out
    }

    pub fn rule(&mut self, color: Rgb) {
        let y = self.y;
        self.push(Op::Path {
            cmds: vec![Cmd::Move(self.left, y), Cmd::Line(self.right, y)],
            fill: None,
            stroke: Some(Pen::solid(color, 0.75)),
        });
    }

    /// Paints a chart scene with its top-left at (`x`, `y`), scaled by `s`.
    pub fn paint(&mut self, scene: &Scene, x: f32, y: f32, s: f32) {
        let map = |px: f32, py: f32| (x + px * s, y + py * s);
        let map_cmd = |c: &Cmd| match *c {
            Cmd::Move(a, b) => {
                let (a, b) = map(a, b);
                Cmd::Move(a, b)
            }
            Cmd::Line(a, b) => {
                let (a, b) = map(a, b);
                Cmd::Line(a, b)
            }
            Cmd::Cubic(a, b, c, d, e, f) => {
                let (a, b) = map(a, b);
                let (c, d) = map(c, d);
                let (e, f) = map(e, f);
                Cmd::Cubic(a, b, c, d, e, f)
            }
            Cmd::Close => Cmd::Close,
        };
        let fill = |f: Option<super::svg::scene::Fill>| {
            f.map(|f| Paint {
                color: f.color,
                alpha: f.opacity,
            })
        };
        let pen = |p: Option<super::svg::scene::Stroke>| {
            p.map(|p| Pen {
                color: p.color,
                width: p.width * s,
                alpha: p.opacity,
                dash: p.dash.map(|(a, b)| (a * s, b * s)),
            })
        };
        if let Some(bg) = scene.background {
            self.push(Op::Rect {
                x,
                y,
                w: scene.width * s,
                h: scene.height * s,
                fill: Some(Paint {
                    color: bg,
                    alpha: 1.0,
                }),
                stroke: None,
            });
        }
        for item in &scene.items {
            match item {
                Item::Rect {
                    x: rx,
                    y: ry,
                    w,
                    h,
                    fill: f,
                    stroke,
                    radius,
                } => {
                    let (px, py) = map(*rx, *ry);
                    if *radius > 0.0 {
                        let cmds = rounded_rect(px, py, w * s, h * s, radius * s);
                        self.push(Op::Path {
                            cmds,
                            fill: fill(*f),
                            stroke: pen(*stroke),
                        });
                    } else {
                        self.push(Op::Rect {
                            x: px,
                            y: py,
                            w: w * s,
                            h: h * s,
                            fill: fill(*f),
                            stroke: pen(*stroke),
                        });
                    }
                }
                Item::Line {
                    x1,
                    y1,
                    x2,
                    y2,
                    stroke,
                } => {
                    let (a, b) = map(*x1, *y1);
                    let (c, d) = map(*x2, *y2);
                    self.push(Op::Path {
                        cmds: vec![Cmd::Move(a, b), Cmd::Line(c, d)],
                        fill: None,
                        stroke: pen(Some(*stroke)),
                    });
                }
                Item::Path {
                    cmds,
                    fill: f,
                    stroke,
                } => {
                    self.push(Op::Path {
                        cmds: cmds.iter().map(map_cmd).collect(),
                        fill: fill(*f),
                        stroke: pen(*stroke),
                    });
                }
                Item::Circle {
                    cx,
                    cy,
                    r,
                    fill: f,
                    stroke,
                } => {
                    let (px, py) = map(*cx, *cy);
                    let mut cmds = super::svg::scene::arc(
                        px,
                        py,
                        r * s,
                        0.0,
                        std::f32::consts::TAU,
                        Cmd::Move(0.0, 0.0),
                    );
                    cmds.push(Cmd::Close);
                    self.push(Op::Path {
                        cmds,
                        fill: fill(*f),
                        stroke: pen(*stroke),
                    });
                }
                Item::Text(t) => {
                    let size = t.size * s;
                    let width = self.measure(&t.text, FontId::Sans, size);
                    let offset = match t.anchor {
                        super::svg::scene::Anchor::Start => 0.0,
                        super::svg::scene::Anchor::Middle => -width / 2.0,
                        super::svg::scene::Anchor::End => -width,
                    };
                    let (px, py) = map(t.x, t.y);
                    let (runs, _) = self.fonts.shape(FontId::Sans, &t.text, size);
                    let angle = t.rotate.to_radians();
                    let (dx, dy) = (offset * angle.cos(), offset * angle.sin());
                    self.push(Op::Text(TextOp {
                        x: px + dx,
                        y: py + dy,
                        size,
                        runs,
                        color: t.color,
                        alpha: 1.0,
                        bold: t.bold,
                        italic: false,
                        rotate: t.rotate,
                    }));
                }
            }
        }
    }

    fn running_heads(&mut self) {
        let total = self.pages.len();
        let title = self.doc.meta.title.clone();
        let generated = self.doc.meta.generated_at.clone().unwrap_or_default();
        let version = self.doc.meta.notebook_version.clone().unwrap_or_default();
        let dim = self.theme.text_dim;
        let border = self.theme.border;
        for i in 0..total {
            let saved = std::mem::take(&mut self.pages[i].ops);
            let links = std::mem::take(&mut self.pages[i].links);
            self.pages.push(PageOps {
                ops: Vec::new(),
                links: Vec::new(),
            });
            let size = 8.0;
            if i >= self.cover_pages {
                let head_y = self.top - 22.0;
                let gen_w = self.measure(&generated, FontId::Sans, size);
                let title_text = self.fit(&title, FontId::Sans, size, self.width() - gen_w - 16.0);
                self.text(
                    self.left,
                    head_y,
                    &title_text,
                    FontId::Sans,
                    size,
                    dim,
                    false,
                    false,
                );
                self.text(
                    self.right - gen_w,
                    head_y,
                    &generated,
                    FontId::Sans,
                    size,
                    dim,
                    false,
                    false,
                );
                self.push(Op::Path {
                    cmds: vec![
                        Cmd::Move(self.left, head_y + 5.0),
                        Cmd::Line(self.right, head_y + 5.0),
                    ],
                    fill: None,
                    stroke: Some(Pen::solid(border, 0.5)),
                });
            }
            let foot_y = self.page_h - 30.0;
            let label = self.labels.page(i + 1, total);
            let lw = self.measure(&label, FontId::Sans, size);
            self.text(
                self.left + (self.width() - lw) / 2.0,
                foot_y,
                &label,
                FontId::Sans,
                size,
                dim,
                false,
                false,
            );
            if !version.is_empty() {
                let v = self.fit(&version, FontId::Mono, 7.0, self.width() / 3.0);
                self.text(self.left, foot_y, &v, FontId::Mono, 7.0, dim, false, false);
            }
            let Some(heads) = self.pages.pop() else {
                continue;
            };
            let mut ops = saved;
            ops.extend(heads.ops);
            self.pages[i].ops = ops;
            self.pages[i].links = links;
        }
    }
}

pub fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Vec<Cmd> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let k = 0.552_284_8 * r;
    vec![
        Cmd::Move(x + r, y),
        Cmd::Line(x + w - r, y),
        Cmd::Cubic(x + w - r + k, y, x + w, y + r - k, x + w, y + r),
        Cmd::Line(x + w, y + h - r),
        Cmd::Cubic(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h),
        Cmd::Line(x + r, y + h),
        Cmd::Cubic(x + r - k, y + h, x, y + h - r + k, x, y + h - r),
        Cmd::Line(x, y + r),
        Cmd::Cubic(x, y + r - k, x + r - k, y, x + r, y),
        Cmd::Close,
    ]
}

/// The report as PDF bytes and what the PDF could not carry.
pub fn render_pdf(
    doc: &ReportDoc,
    options: &RenderOptions,
) -> Result<(Vec<u8>, FidelityReport), String> {
    let mut layout = Layout::new(doc, options)?;
    layout.cover();
    for block in &doc.blocks {
        layout.block(block);
    }
    layout.appendix();
    layout.running_heads();
    if !layout.fonts.missing.is_empty() {
        let sample: String = layout.fonts.missing.iter().take(8).collect();
        let count = layout.fonts.missing.len();
        layout
            .fidelity
            .note(FidelityKind::GlyphsMissing { sample, count }, None);
    }
    if layout.fonts.complex {
        layout.fidelity.note_once(FidelityKind::ComplexScripts);
    }
    if options.theme == super::theme::ThemeMode::Dark {
        layout.fidelity.note_once(FidelityKind::FixedTheme);
    }
    let bytes = write::write(&layout)?;
    Ok((bytes, layout.fidelity))
}

/// Sequential object ids.
pub(crate) struct Ids(i32);

impl Ids {
    pub fn new() -> Self {
        Ids(1)
    }

    pub fn next(&mut self) -> Ref {
        let r = Ref::new(self.0);
        self.0 += 1;
        r
    }
}

#[cfg(test)]
mod tests;
