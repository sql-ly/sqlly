//! Serializing laid-out pages with pdf-writer.

use std::collections::BTreeMap;

use pdf_writer::types::{ActionType, AnnotationType, PageMode, TextRenderingMode};
use pdf_writer::{Content, Date, Filter, Finish, Name, Pdf, Rect, Ref, Str, TextStr};

use super::fonts::{Embedded, FontId};
use super::{Ids, Layout, Op, Paint, Pen};
use crate::report::svg::scene::Cmd;

fn compress(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

fn alpha_key(alpha: f32) -> u8 {
    (alpha.clamp(0.0, 1.0) * 100.0).round() as u8
}

fn date(text: &str) -> Option<Date> {
    let t = crate::report::format::parse_temporal(text)?.as_datetime();
    use chrono::{Datelike, Timelike};
    Some(
        Date::new(u16::try_from(t.year()).ok()?)
            .month(t.month() as u8)
            .day(t.day() as u8)
            .hour(t.hour() as u8)
            .minute(t.minute() as u8)
            .second(t.second() as u8)
            .utc_offset_hour(0)
            .utc_offset_minute(0),
    )
}

struct Painter<'a> {
    content: Content,
    page_h: f32,
    fonts: &'a BTreeMap<FontId, Embedded>,
    gstates: &'a BTreeMap<u8, String>,
}

impl Painter<'_> {
    fn y(&self, y: f32) -> f32 {
        self.page_h - y
    }

    fn alpha(&mut self, fill: Option<f32>, stroke: Option<f32>) {
        let a = fill.or(stroke).unwrap_or(1.0);
        if let Some(name) = self.gstates.get(&alpha_key(a)) {
            self.content.set_parameters(Name(name.as_bytes()));
        }
    }

    fn pen(&mut self, pen: &Pen) {
        let (r, g, b) = pen.color.unit();
        self.content.set_stroke_rgb(r, g, b);
        self.content.set_line_width(pen.width.max(0.1));
        match pen.dash {
            Some((on, off)) => self.content.set_dash_pattern([on, off], 0.0),
            None => self.content.set_dash_pattern([], 0.0),
        };
    }

    fn fill(&mut self, paint: &Paint) {
        let (r, g, b) = paint.color.unit();
        self.content.set_fill_rgb(r, g, b);
    }

    fn finish_path(&mut self, fill: Option<&Paint>, stroke: Option<&Pen>) {
        match (fill, stroke) {
            (Some(_), Some(_)) => self.content.fill_nonzero_and_stroke(),
            (Some(_), None) => self.content.fill_nonzero(),
            (None, Some(_)) => self.content.stroke(),
            (None, None) => self.content.end_path(),
        };
    }

    fn op(&mut self, op: &Op) {
        match op {
            Op::Rect {
                x,
                y,
                w,
                h,
                fill,
                stroke,
            } => {
                self.content.save_state();
                self.alpha(fill.map(|f| f.alpha), stroke.map(|s| s.alpha));
                if let Some(f) = fill {
                    self.fill(f);
                }
                if let Some(s) = stroke {
                    self.pen(s);
                }
                self.content.rect(*x, self.y(y + h), *w, *h);
                self.finish_path(fill.as_ref(), stroke.as_ref());
                self.content.restore_state();
            }
            Op::Path { cmds, fill, stroke } => {
                if cmds.is_empty() {
                    return;
                }
                self.content.save_state();
                self.alpha(fill.map(|f| f.alpha), stroke.map(|s| s.alpha));
                if let Some(f) = fill {
                    self.fill(f);
                }
                if let Some(s) = stroke {
                    self.pen(s);
                    self.content
                        .set_line_join(pdf_writer::types::LineJoinStyle::RoundJoin);
                    self.content
                        .set_line_cap(pdf_writer::types::LineCapStyle::RoundCap);
                }
                for cmd in cmds {
                    match *cmd {
                        Cmd::Move(x, y) => self.content.move_to(x, self.y(y)),
                        Cmd::Line(x, y) => self.content.line_to(x, self.y(y)),
                        Cmd::Cubic(a, b, c, d, e, f) => {
                            self.content
                                .cubic_to(a, self.y(b), c, self.y(d), e, self.y(f))
                        }
                        Cmd::Close => self.content.close_path(),
                    };
                }
                self.finish_path(fill.as_ref(), stroke.as_ref());
                self.content.restore_state();
            }
            Op::Image { x, y, w, h, index } => {
                self.content.save_state();
                self.content
                    .transform([*w, 0.0, 0.0, *h, *x, self.y(y + h)]);
                self.content
                    .x_object(Name(format!("Im{}", index + 1).as_bytes()));
                self.content.restore_state();
            }
            Op::Text(t) => {
                self.content.save_state();
                self.alpha(Some(t.alpha), None);
                let (r, g, b) = t.color.unit();
                self.content.set_fill_rgb(r, g, b);
                if t.bold {
                    self.content.set_stroke_rgb(r, g, b);
                    self.content.set_line_width(t.size * 0.045);
                }
                let angle = -t.rotate.to_radians();
                let (cos, sin) = (angle.cos(), angle.sin());
                let shear = if t.italic { 0.2 } else { 0.0 };
                self.content.begin_text();
                if t.bold {
                    self.content
                        .set_text_rendering_mode(TextRenderingMode::FillStroke);
                }
                for run in &t.runs {
                    let Some(embedded) = self.fonts.get(&run.font) else {
                        continue;
                    };
                    let mut bytes = Vec::with_capacity(run.glyphs.len() * 2);
                    for g in &run.glyphs {
                        let id = embedded.remap.get(*g).unwrap_or(0);
                        bytes.extend_from_slice(&id.to_be_bytes());
                    }
                    let ox = t.x + run.dx * cos;
                    let oy = self.y(t.y) + run.dx * sin;
                    self.content.set_font(Name(run.font.resource()), t.size);
                    self.content.set_text_matrix([
                        cos,
                        sin,
                        -sin + shear * cos,
                        cos + shear * sin,
                        ox,
                        oy,
                    ]);
                    self.content.show(Str(&bytes));
                }
                self.content.end_text();
                self.content.restore_state();
            }
        }
    }
}

pub(super) fn write(layout: &Layout) -> Result<Vec<u8>, String> {
    let mut ids = Ids::new();
    let catalog_id = ids.next();
    let pages_id = ids.next();
    let info_id = ids.next();
    let mut pdf = Pdf::new();

    let mut fonts: BTreeMap<FontId, Embedded> = BTreeMap::new();
    let mut font_refs: BTreeMap<FontId, Ref> = BTreeMap::new();
    for font in [FontId::Sans, FontId::Mono] {
        if layout.fonts.is_used(font) {
            let refs = [ids.next(), ids.next(), ids.next(), ids.next(), ids.next()];
            let embedded = layout.fonts.write(&mut pdf, font, refs)?;
            font_refs.insert(font, refs[0]);
            fonts.insert(font, embedded);
        }
    }

    let mut alphas: std::collections::BTreeSet<u8> = Default::default();
    for page in &layout.pages {
        for op in &page.ops {
            let a = match op {
                Op::Rect { fill, stroke, .. } | Op::Path { fill, stroke, .. } => {
                    fill.map(|f| f.alpha).or(stroke.map(|s| s.alpha))
                }
                Op::Text(t) => Some(t.alpha),
                Op::Image { .. } => None,
            };
            if let Some(a) = a {
                alphas.insert(alpha_key(a));
            }
        }
    }
    let mut gstates: BTreeMap<u8, String> = BTreeMap::new();
    let mut gstate_refs: Vec<(String, Ref)> = Vec::new();
    for a in alphas {
        let id = ids.next();
        let name = format!("GS{a}");
        pdf.ext_graphics(id)
            .non_stroking_alpha(f32::from(a) / 100.0)
            .stroking_alpha(f32::from(a) / 100.0);
        gstates.insert(a, name.clone());
        gstate_refs.push((name, id));
    }

    let mut image_refs: Vec<(String, Ref)> = Vec::new();
    for (i, raster) in layout.images.iter().enumerate() {
        let id = ids.next();
        let smask = raster.alpha.as_ref().map(|alpha| {
            let mask_id = ids.next();
            let data = compress(alpha);
            let mut mask = pdf.image_xobject(mask_id, &data);
            mask.width(raster.width as i32)
                .height(raster.height as i32)
                .bits_per_component(8);
            mask.filter(Filter::FlateDecode);
            mask.color_space().device_gray();
            mask.finish();
            mask_id
        });
        let data = compress(&raster.rgb);
        let mut image = pdf.image_xobject(id, &data);
        image
            .width(raster.width as i32)
            .height(raster.height as i32)
            .bits_per_component(8);
        image.filter(Filter::FlateDecode);
        image.color_space().device_rgb();
        if let Some(mask) = smask {
            image.s_mask(mask);
        }
        image.finish();
        image_refs.push((format!("Im{}", i + 1), id));
    }

    let page_ids: Vec<Ref> = (0..layout.pages.len()).map(|_| ids.next()).collect();
    for (i, page) in layout.pages.iter().enumerate() {
        let mut painter = Painter {
            content: Content::new(),
            page_h: layout.page_h,
            fonts: &fonts,
            gstates: &gstates,
        };
        for op in &page.ops {
            painter.op(op);
        }
        let stream = compress(&painter.content.finish());
        let content_id = ids.next();
        pdf.stream(content_id, &stream).filter(Filter::FlateDecode);
        let mut annots = Vec::new();
        for (rect, url) in &page.links {
            let id = ids.next();
            let [x, y, w, h] = *rect;
            let mut annot = pdf.annotation(id);
            annot
                .subtype(AnnotationType::Link)
                .rect(Rect::new(
                    x,
                    layout.page_h - y - h,
                    x + w,
                    layout.page_h - y,
                ))
                .border(0.0, 0.0, 0.0, None);
            annot
                .action()
                .action_type(ActionType::Uri)
                .uri(Str(url.as_bytes()));
            annot.finish();
            annots.push(id);
        }
        let mut writer = pdf.page(page_ids[i]);
        writer
            .parent(pages_id)
            .media_box(Rect::new(0.0, 0.0, layout.page_w, layout.page_h))
            .contents(content_id);
        if !annots.is_empty() {
            writer.annotations(annots);
        }
        let mut resources = writer.resources();
        {
            let mut f = resources.fonts();
            for (font, id) in &font_refs {
                f.pair(Name(font.resource()), *id);
            }
        }
        if !gstate_refs.is_empty() {
            let mut g = resources.ext_g_states();
            for (name, id) in &gstate_refs {
                g.pair(Name(name.as_bytes()), *id);
            }
        }
        if !image_refs.is_empty() {
            let mut x = resources.x_objects();
            for (name, id) in &image_refs {
                x.pair(Name(name.as_bytes()), *id);
            }
        }
        resources.finish();
        writer.finish();
    }
    pdf.pages(pages_id)
        .kids(page_ids.iter().copied())
        .count(page_ids.len() as i32);

    let outline_id =
        (!layout.outline.is_empty()).then(|| write_outline(&mut pdf, &mut ids, layout, &page_ids));

    {
        let mut info = pdf.document_info(info_id);
        info.title(TextStr(&layout.doc.meta.title));
        if let Some(a) = layout.doc.meta.author.as_deref().filter(|a| !a.is_empty()) {
            info.author(TextStr(a));
        }
        if let Some(s) = layout
            .doc
            .meta
            .subtitle
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            info.subject(TextStr(s));
        }
        info.creator(TextStr("SQLLY"));
        info.producer(TextStr("SQLLY notebook report"));
        if let Some(d) = layout.doc.meta.generated_at.as_deref().and_then(date) {
            info.creation_date(d);
        }
    }
    let mut catalog = pdf.catalog(catalog_id);
    catalog.pages(pages_id);
    if let Some(id) = outline_id {
        catalog.outlines(id);
        catalog.page_mode(PageMode::UseOutlines);
    }
    catalog.lang(TextStr(&layout.options.language));
    catalog.finish();
    Ok(pdf.finish())
}

#[allow(clippy::needless_range_loop)]
fn write_outline(pdf: &mut Pdf, ids: &mut Ids, layout: &Layout, page_ids: &[Ref]) -> Ref {
    let root = ids.next();
    let entries = &layout.outline;
    let refs: Vec<Ref> = entries.iter().map(|_| ids.next()).collect();
    let min_level = entries.iter().map(|e| e.level).min().unwrap_or(1);
    let level = |i: usize| entries[i].level.saturating_sub(min_level);
    let mut parent: Vec<Option<usize>> = vec![None; entries.len()];
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..entries.len() {
        while stack.last().is_some_and(|&p| level(p) >= level(i)) {
            stack.pop();
        }
        parent[i] = stack.last().copied();
        stack.push(i);
    }
    let children = |p: Option<usize>| -> Vec<usize> {
        (0..entries.len()).filter(|&i| parent[i] == p).collect()
    };
    fn descendants(i: usize, parent: &[Option<usize>]) -> i32 {
        (0..parent.len())
            .filter(|&c| parent[c] == Some(i))
            .map(|c| 1 + descendants(c, parent))
            .sum()
    }
    for i in 0..entries.len() {
        let siblings = children(parent[i]);
        let pos = siblings.iter().position(|&s| s == i).unwrap_or(0);
        let kids = children(Some(i));
        let entry = &entries[i];
        let mut item = pdf.outline_item(refs[i]);
        item.title(TextStr(&entry.title));
        item.parent(parent[i].map(|p| refs[p]).unwrap_or(root));
        if pos > 0 {
            item.prev(refs[siblings[pos - 1]]);
        }
        if pos + 1 < siblings.len() {
            item.next(refs[siblings[pos + 1]]);
        }
        if let (Some(first), Some(last)) = (kids.first(), kids.last()) {
            item.first(refs[*first]);
            item.last(refs[*last]);
            item.count(descendants(i, &parent));
        }
        item.dest()
            .page(page_ids[entry.page.min(page_ids.len() - 1)])
            .xyz(layout.left, layout.page_h - entry.y + 4.0, None);
    }
    let top = children(None);
    let mut outline = pdf.outline(root);
    if let (Some(first), Some(last)) = (top.first(), top.last()) {
        outline.first(refs[*first]);
        outline.last(refs[*last]);
    }
    outline.count(entries.len() as i32);
    root
}
