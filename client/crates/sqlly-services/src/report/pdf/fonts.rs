//! The two faces embedded in report PDFs (Noto Sans for text, JetBrains
//! Mono for code, both SIL OFL 1.1, licenses beside the files), glyph
//! lookup with fallback from one to the other, advance widths, and the
//! subset CID fonts written into the file.

use std::collections::BTreeSet;

use pdf_writer::types::{CidFontType, FontFlags, SystemInfo, UnicodeCmap};
use pdf_writer::{Filter, Name, Pdf, Rect, Ref, Str};
use skrifa::instance::{LocationRef, Size};
use skrifa::{GlyphId, MetadataProvider};

pub const SANS_FONT: &[u8] = include_bytes!("../fonts/NotoSans-Regular-subset.ttf");
pub const MONO_FONT: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FontId {
    Sans,
    Mono,
}

impl FontId {
    pub fn index(self) -> usize {
        match self {
            FontId::Sans => 0,
            FontId::Mono => 1,
        }
    }

    fn other(self) -> FontId {
        match self {
            FontId::Sans => FontId::Mono,
            FontId::Mono => FontId::Sans,
        }
    }

    pub fn resource(self) -> &'static [u8] {
        match self {
            FontId::Sans => b"F1",
            FontId::Mono => b"F2",
        }
    }

    fn base_name(self) -> &'static str {
        match self {
            FontId::Sans => "NotoSans-Regular",
            FontId::Mono => "JetBrainsMono-Regular",
        }
    }
}

/// Glyphs drawn in one face, with their x offset (points) from the start
/// of the text.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub font: FontId,
    pub glyphs: Vec<u16>,
    pub dx: f32,
}

struct Face {
    font: skrifa::FontRef<'static>,
    upem: f32,
    metrics: skrifa::metrics::Metrics,
}

pub struct Fonts {
    faces: [Face; 2],
    used: [BTreeSet<(u16, char)>; 2],
    pub missing: BTreeSet<char>,
    pub complex: bool,
}

fn is_complex(c: char) -> bool {
    matches!(c as u32, 0x0590..=0x08FF | 0x0900..=0x0DFF | 0x0E00..=0x0FFF | 0x1000..=0x109F | 0x1780..=0x17FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF)
}

impl Fonts {
    pub fn new() -> Result<Self, String> {
        let load = |data: &'static [u8]| -> Result<Face, String> {
            let font = skrifa::FontRef::new(data).map_err(|e| e.to_string())?;
            let metrics = font.metrics(Size::unscaled(), LocationRef::default());
            let upem = f32::from(metrics.units_per_em);
            Ok(Face {
                font,
                upem,
                metrics,
            })
        };
        Ok(Self {
            faces: [load(SANS_FONT)?, load(MONO_FONT)?],
            used: [BTreeSet::new(), BTreeSet::new()],
            missing: BTreeSet::new(),
            complex: false,
        })
    }

    fn lookup(&self, preferred: FontId, c: char) -> (FontId, u16) {
        for font in [preferred, preferred.other()] {
            if let Some(g) = self.faces[font.index()]
                .font
                .charmap()
                .map(c)
                .and_then(|g| u16::try_from(g.to_u32()).ok())
            {
                return (font, g);
            }
        }
        (preferred, 0)
    }

    fn advance(&self, font: FontId, gid: u16) -> f32 {
        let face = &self.faces[font.index()];
        face.font
            .glyph_metrics(Size::unscaled(), LocationRef::default())
            .advance_width(GlyphId::new(u32::from(gid)))
            .unwrap_or(0.0)
            / face.upem
    }

    /// Width of `text` at `size` points, without recording glyph use.
    pub fn width(&self, preferred: FontId, text: &str, size: f32) -> f32 {
        text.chars()
            .filter(|c| !c.is_control())
            .map(|c| {
                let (font, gid) = self.lookup(preferred, c);
                self.advance(font, gid)
            })
            .sum::<f32>()
            * size
    }

    /// Glyph runs for `text` and its width at `size`, recording every glyph
    /// for the subset (and characters no face has).
    pub fn shape(&mut self, preferred: FontId, text: &str, size: f32) -> (Vec<Run>, f32) {
        let mut runs: Vec<Run> = Vec::new();
        let mut x = 0.0;
        for c in text.chars() {
            if c.is_control() {
                continue;
            }
            if is_complex(c) {
                self.complex = true;
            }
            let (font, gid) = self.lookup(preferred, c);
            if gid == 0 && !c.is_whitespace() {
                self.missing.insert(c);
            }
            self.used[font.index()].insert((gid, c));
            match runs.last_mut() {
                Some(run) if run.font == font => run.glyphs.push(gid),
                _ => runs.push(Run {
                    font,
                    glyphs: vec![gid],
                    dx: x,
                }),
            }
            x += self.advance(font, gid) * size;
        }
        (runs, x)
    }

    /// Ascender and descender as fractions of the em.
    pub fn metrics(&self, font: FontId) -> (f32, f32) {
        let face = &self.faces[font.index()];
        (
            face.metrics.ascent / face.upem,
            face.metrics.descent / face.upem,
        )
    }

    pub fn is_used(&self, font: FontId) -> bool {
        !self.used[font.index()].is_empty()
    }
}

/// The written font: old glyph id → id in the subset.
pub struct Embedded {
    pub remap: subsetter::GlyphRemapper,
}

fn compress(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

/// A six-letter subset tag derived from the glyph set, so the same glyphs
/// always get the same name.
fn subset_tag(glyphs: &BTreeSet<(u16, char)>, font: FontId) -> String {
    let mut hash: u32 = 0x811C_9DC5 ^ font.index() as u32;
    for (g, _) in glyphs {
        for b in g.to_be_bytes() {
            hash ^= u32::from(b);
            hash = hash.wrapping_mul(0x0100_0193);
        }
    }
    (0..6)
        .map(|i| char::from(b'A' + ((hash >> (i * 5)) % 26) as u8))
        .collect()
}

impl Fonts {
    /// Writes the subset of `font` as a Type 0 / CIDFontType2 font with a
    /// ToUnicode map. `ids` are four free object ids: font, CID font,
    /// descriptor, font file; `cmap_id` the ToUnicode stream.
    pub fn write(&self, pdf: &mut Pdf, font: FontId, ids: [Ref; 5]) -> Result<Embedded, String> {
        let [type0_id, cid_id, descriptor_id, file_id, cmap_id] = ids;
        let used = &self.used[font.index()];
        let gids: Vec<u16> = used.iter().map(|(g, _)| *g).collect();
        let remap = subsetter::GlyphRemapper::new_from_glyphs_sorted(&gids);
        let data = match font {
            FontId::Sans => SANS_FONT,
            FontId::Mono => MONO_FONT,
        };
        let subset = subsetter::subset(data, 0, &remap).map_err(|e| e.to_string())?;
        let tag = subset_tag(used, font);
        let base = format!("{tag}+{}", font.base_name());
        let system_info = SystemInfo {
            registry: Str(b"Adobe"),
            ordering: Str(b"Identity"),
            supplement: 0,
        };
        pdf.type0_font(type0_id)
            .base_font(Name(base.as_bytes()))
            .encoding_predefined(Name(b"Identity-H"))
            .descendant_font(cid_id)
            .to_unicode(cmap_id);
        let face = &self.faces[font.index()];
        let scale = 1000.0 / face.upem;
        let widths: Vec<f32> = remap
            .remapped_gids()
            .map(|old| self.advance(font, old) * 1000.0)
            .collect();
        {
            let mut cid = pdf.cid_font(cid_id);
            cid.subtype(CidFontType::Type2)
                .base_font(Name(base.as_bytes()))
                .system_info(system_info)
                .font_descriptor(descriptor_id)
                .default_width(0.0)
                .cid_to_gid_map_predefined(Name(b"Identity"));
            cid.widths().consecutive(0, widths);
        }
        let bbox = face.metrics.bounds.unwrap_or_default();
        let (ascent, descent) = self.metrics(font);
        pdf.font_descriptor(descriptor_id)
            .name(Name(base.as_bytes()))
            .flags(
                FontFlags::NON_SYMBOLIC
                    | if font == FontId::Mono {
                        FontFlags::FIXED_PITCH
                    } else {
                        FontFlags::empty()
                    },
            )
            .bbox(Rect::new(
                bbox.x_min * scale,
                bbox.y_min * scale,
                bbox.x_max * scale,
                bbox.y_max * scale,
            ))
            .italic_angle(0.0)
            .ascent(ascent * 1000.0)
            .descent(descent * 1000.0)
            .cap_height(face.metrics.cap_height.unwrap_or(700.0) * scale)
            .stem_v(80.0)
            .font_file2(file_id);
        let compressed = compress(&subset);
        pdf.stream(file_id, &compressed)
            .filter(Filter::FlateDecode)
            .pair(Name(b"Length1"), subset.len() as i32);
        let mut cmap = UnicodeCmap::new(Name(b"Custom"), system_info);
        for (old, c) in used {
            if let Some(new) = remap.get(*old)
                && *old != 0
            {
                cmap.pair(new, *c);
            }
        }
        let cmap_data = cmap.finish();
        pdf.cmap(cmap_id, &cmap_data);
        Ok(Embedded { remap })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_comes_from_sans_and_cyrillic_falls_back_to_mono() {
        let mut fonts = Fonts::new().unwrap();
        let (runs, width) = fonts.shape(FontId::Sans, "Hi Привет", 10.0);
        assert_eq!(runs[0].font, FontId::Sans);
        assert!(runs.iter().any(|r| r.font == FontId::Mono));
        assert!(width > 0.0);
        assert!(fonts.missing.is_empty());
        fonts.shape(FontId::Sans, "日本", 10.0);
        assert!(fonts.missing.contains(&'日'));
        assert!((fonts.width(FontId::Sans, "Hi", 10.0) - 10.0 * (0.741 + 0.258)).abs() < 0.05);
    }
}
