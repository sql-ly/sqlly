//! Annotation model and pixel compositing for the Send Feedback dialog.
//!
//! Everything here is pure and UI-free so it can be unit-tested: the
//! annotation document (arrows + privacy blackouts, selection, undo/redo),
//! pointer-drag interactions (create, move, resize by handle, drag an arrow
//! end), hit testing, and the final flatten that burns the annotations into
//! the screenshot at native resolution before it is encoded and uploaded.
//!
//! All coordinates are screenshot pixels (the native / Retina backing
//! resolution), never window points — the dialog maps pointer positions into
//! this space and back when it paints.
//!
//! Privacy invariant: [`flatten`] paints blackouts last and covers every
//! pixel a blackout touches (even partially) with opaque black, so no pixel
//! under a blackout survives into the uploaded PNG.

use image::{Rgba, RgbaImage};

/// A point in screenshot pixel space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

impl Pt {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    fn offset(self, dx: f32, dy: f32) -> Self {
        Self::new(self.x + dx, self.y + dy)
    }

    fn distance(self, other: Pt) -> f32 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }
}

/// An axis-aligned rectangle in screenshot pixel space. Always normalized
/// (non-negative width and height) once built through [`Rect::from_corners`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// The rectangle spanned by two opposite corners, in any order.
    pub fn from_corners(a: Pt, b: Pt) -> Self {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        Self::new(x, y, (a.x - b.x).abs(), (a.y - b.y).abs())
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, p: Pt) -> bool {
        p.x >= self.x && p.x <= self.right() && p.y >= self.y && p.y <= self.bottom()
    }

    fn translated(self, dx: f32, dy: f32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// Shift (never shrink) the rectangle so it lies inside `w × h` where it
    /// fits; a rectangle larger than the image is pinned to the origin.
    fn kept_inside(self, w: f32, h: f32) -> Self {
        let x = self.x.clamp(0.0, (w - self.w).max(0.0));
        let y = self.y.clamp(0.0, (h - self.h).max(0.0));
        Self::new(x, y, self.w, self.h)
    }
}

/// One mark on the screenshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Annotation {
    /// An arrow from `from` (tail) to `to` (the head points here).
    Arrow { from: Pt, to: Pt },
    /// An opaque black privacy box.
    Blackout(Rect),
}

/// The active tool in the dialog's toolbar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// Click to select; drag to move; drag handles to resize.
    Select,
    /// Drag on empty space to draw a new arrow.
    Arrow,
    /// Drag on empty space to draw a new blackout.
    Blackout,
}

/// A resize handle on a selected blackout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Handle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl Handle {
    pub const ALL: [Handle; 8] = [
        Handle::TopLeft,
        Handle::Top,
        Handle::TopRight,
        Handle::Right,
        Handle::BottomRight,
        Handle::Bottom,
        Handle::BottomLeft,
        Handle::Left,
    ];

    fn moves_left(self) -> bool {
        matches!(self, Self::TopLeft | Self::Left | Self::BottomLeft)
    }

    fn moves_right(self) -> bool {
        matches!(self, Self::TopRight | Self::Right | Self::BottomRight)
    }

    fn moves_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::Top | Self::TopRight)
    }

    fn moves_bottom(self) -> bool {
        matches!(self, Self::BottomLeft | Self::Bottom | Self::BottomRight)
    }
}

/// Center of `handle` on `rect`: the four corners and the four edge midpoints.
pub fn handle_center(rect: &Rect, handle: Handle) -> Pt {
    let cx = rect.x + rect.w / 2.0;
    let cy = rect.y + rect.h / 2.0;
    match handle {
        Handle::TopLeft => Pt::new(rect.x, rect.y),
        Handle::Top => Pt::new(cx, rect.y),
        Handle::TopRight => Pt::new(rect.right(), rect.y),
        Handle::Right => Pt::new(rect.right(), cy),
        Handle::BottomRight => Pt::new(rect.right(), rect.bottom()),
        Handle::Bottom => Pt::new(cx, rect.bottom()),
        Handle::BottomLeft => Pt::new(rect.x, rect.bottom()),
        Handle::Left => Pt::new(rect.x, cy),
    }
}

/// The square grab area of `handle`, `size` pixels on a side, centered on
/// [`handle_center`].
pub fn handle_rect(rect: &Rect, handle: Handle, size: f32) -> Rect {
    let c = handle_center(rect, handle);
    Rect::new(c.x - size / 2.0, c.y - size / 2.0, size, size)
}

/// Resize `original` by dragging `handle` by `(dx, dy)`. Only the edges the
/// handle controls move; an edge can never cross (or come within `min` of)
/// the opposite edge, so the box never inverts or collapses. The result is
/// clamped to the `bounds` (image width, height).
pub fn resize_rect(
    original: Rect,
    handle: Handle,
    dx: f32,
    dy: f32,
    min: f32,
    bounds: (f32, f32),
) -> Rect {
    let (bw, bh) = bounds;
    let mut left = original.x;
    let mut top = original.y;
    let mut right = original.right();
    let mut bottom = original.bottom();
    if handle.moves_left() {
        left = (left + dx).clamp(0.0, (right - min).max(0.0));
    }
    if handle.moves_right() {
        right = (right + dx).clamp((left + min).min(bw), bw);
    }
    if handle.moves_top() {
        top = (top + dy).clamp(0.0, (bottom - min).max(0.0));
    }
    if handle.moves_bottom() {
        bottom = (bottom + dy).clamp((top + min).min(bh), bh);
    }
    Rect::new(left, top, right - left, bottom - top)
}

/// Shortest distance from `p` to the segment `a`–`b`.
pub fn distance_to_segment(p: Pt, a: Pt, b: Pt) -> f32 {
    let (vx, vy) = (b.x - a.x, b.y - a.y);
    let len2 = vx * vx + vy * vy;
    if len2 <= f32::EPSILON {
        return p.distance(a);
    }
    let t = (((p.x - a.x) * vx + (p.y - a.y) * vy) / len2).clamp(0.0, 1.0);
    p.distance(Pt::new(a.x + t * vx, a.y + t * vy))
}

/// Which end of an arrow a drag grabbed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowEnd {
    Tail,
    Head,
}

/// What a pointer position lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// A resize handle of the selected blackout.
    Handle(usize, Handle),
    /// An end point of the selected arrow.
    ArrowEnd(usize, ArrowEnd),
    /// Anywhere on an annotation (blackout interior or near an arrow).
    Body(usize),
}

/// The arrow's drawable geometry: the shaft segment and the filled head
/// triangle (tip first). Shared by the on-screen painter and [`flatten`] so
/// what the user sees is exactly what is burned into the PNG.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArrowGeometry {
    pub shaft: (Pt, Pt),
    pub head: [Pt; 3],
}

/// Geometry for an arrow of stroke `width` from `from` to `to`. `None` for a
/// zero-length arrow (nothing to draw).
pub fn arrow_geometry(from: Pt, to: Pt, width: f32) -> Option<ArrowGeometry> {
    let len = from.distance(to);
    if len < 0.5 {
        return None;
    }
    let (ux, uy) = ((to.x - from.x) / len, (to.y - from.y) / len);
    // Head proportions scale with the stroke; a very short arrow gets a
    // proportionally smaller head so the head never overshoots the tail.
    let head_len = (width * 5.0).min(len);
    let half = head_len * 0.6;
    let base = Pt::new(to.x - ux * head_len, to.y - uy * head_len);
    let (nx, ny) = (-uy, ux);
    // The shaft stops inside the head so the round-ish shaft end never pokes
    // out past the head's sides at the tip.
    let shaft_end = Pt::new(to.x - ux * head_len * 0.6, to.y - uy * head_len * 0.6);
    Some(ArrowGeometry {
        shaft: (from, shaft_end),
        head: [
            to,
            base.offset(nx * half, ny * half),
            base.offset(-nx * half, -ny * half),
        ],
    })
}

/// Arrow stroke width in screenshot pixels for a display scale factor: 4.5
/// points, so arrows read the same on Retina and standard displays.
pub fn arrow_width_for_scale(scale_factor: f32) -> f32 {
    (4.5 * scale_factor.max(1.0)).round()
}

/// The 1-based number of every arrow in `items`, in document order (`None`
/// for blackouts). The badge at each arrow's tail shows this number so the
/// message can refer to "#2", and the upload's annotation metadata carries
/// the same number.
pub fn arrow_numbers(items: &[Annotation]) -> Vec<Option<u32>> {
    let mut next = 0;
    items
        .iter()
        .map(|item| match item {
            Annotation::Arrow { .. } => {
                next += 1;
                Some(next)
            }
            Annotation::Blackout(_) => None,
        })
        .collect()
}

/// A numbered badge: a filled disc with the number drawn in strokes. Shared
/// by the on-screen painter and [`flatten`], like [`ArrowGeometry`].
#[derive(Clone, Debug, PartialEq)]
pub struct BadgeGeometry {
    pub center: Pt,
    pub radius: f32,
    /// Polylines tracing the digits.
    pub strokes: Vec<Vec<Pt>>,
    pub stroke_width: f32,
}

/// Seven-segment-style digit outlines in a unit cell (x 0..1, y 0..1, y
/// down): simple strokes that rasterize crisply at any size without a font.
fn digit_strokes(digit: u32) -> &'static [&'static [(f32, f32)]] {
    const TOP_L: (f32, f32) = (0.0, 0.0);
    const TOP_R: (f32, f32) = (1.0, 0.0);
    const MID_L: (f32, f32) = (0.0, 0.5);
    const MID_R: (f32, f32) = (1.0, 0.5);
    const BOT_L: (f32, f32) = (0.0, 1.0);
    const BOT_R: (f32, f32) = (1.0, 1.0);
    match digit {
        0 => &[&[TOP_L, TOP_R, BOT_R, BOT_L, TOP_L]],
        1 => &[&[(0.2, 0.2), (0.55, 0.0), (0.55, 1.0)]],
        2 => &[&[TOP_L, TOP_R, MID_R, MID_L, BOT_L, BOT_R]],
        3 => &[&[TOP_L, TOP_R, BOT_R, BOT_L], &[MID_L, MID_R]],
        4 => &[&[TOP_L, MID_L, MID_R], &[TOP_R, BOT_R]],
        5 => &[&[TOP_R, TOP_L, MID_L, MID_R, BOT_R, BOT_L]],
        6 => &[&[TOP_R, TOP_L, BOT_L, BOT_R, MID_R, MID_L]],
        7 => &[&[TOP_L, TOP_R, BOT_R]],
        8 => &[&[TOP_L, TOP_R, BOT_R, BOT_L, TOP_L], &[MID_L, MID_R]],
        _ => &[&[MID_R, MID_L, TOP_L, TOP_R, BOT_R, BOT_L]],
    }
}

/// Badge for arrow `number`, centered on `anchor` (the arrow's tail) but
/// kept fully inside a `bounds_w` × `bounds_h` image. Sized from the arrow
/// stroke so it scales with the display.
pub fn badge_geometry(
    anchor: Pt,
    number: u32,
    arrow_width: f32,
    bounds_w: f32,
    bounds_h: f32,
) -> BadgeGeometry {
    let digits: Vec<u32> = number
        .to_string()
        .chars()
        .filter_map(|c| c.to_digit(10))
        .collect();
    let radius = arrow_width * if digits.len() > 1 { 2.9 } else { 2.5 };
    let clamp = |v: f32, max: f32| {
        if max >= radius * 2.0 {
            v.clamp(radius, max - radius)
        } else {
            max / 2.0
        }
    };
    let center = Pt::new(clamp(anchor.x, bounds_w), clamp(anchor.y, bounds_h));
    let glyph_h = radius * 0.95;
    let glyph_w = glyph_h * 0.55;
    let gap = glyph_w * 0.45;
    let total_w = glyph_w * digits.len() as f32 + gap * digits.len().saturating_sub(1) as f32;
    let left = center.x - total_w / 2.0;
    let top = center.y - glyph_h / 2.0;
    let strokes = digits
        .iter()
        .enumerate()
        .flat_map(|(i, &d)| {
            let x0 = left + i as f32 * (glyph_w + gap);
            digit_strokes(d).iter().map(move |line| {
                line.iter()
                    .map(|&(x, y)| Pt::new(x0 + x * glyph_w, top + y * glyph_h))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    BadgeGeometry {
        center,
        radius,
        strokes,
        stroke_width: (radius * 0.2).max(1.5),
    }
}

/// Badge ink (the digits) — white on the arrow-red disc.
pub const BADGE_INK_RGBA: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

/// Rasterize a badge: an anti-aliased disc in `fill`, then the digits in
/// `ink`.
pub fn draw_badge(img: &mut RgbaImage, badge: &BadgeGeometry, fill: [u8; 4], ink: [u8; 4]) {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let r = badge.radius;
    let min_x = (badge.center.x - r - 1.0).max(0.0);
    let max_x = (badge.center.x + r + 1.0).min(w - 1.0);
    let min_y = (badge.center.y - r - 1.0).max(0.0);
    let max_y = (badge.center.y + r + 1.0).min(h - 1.0);
    if min_x > max_x || min_y > max_y {
        return;
    }
    let half = badge.stroke_width / 2.0;
    for y in (min_y as u32)..=(max_y as u32) {
        for x in (min_x as u32)..=(max_x as u32) {
            let p = Pt::new(x as f32 + 0.5, y as f32 + 0.5);
            let disc = (r + 0.5 - p.distance(badge.center)).clamp(0.0, 1.0);
            if disc <= 0.0 {
                continue;
            }
            blend(img.get_pixel_mut(x, y), fill, disc);
            let glyph = badge
                .strokes
                .iter()
                .flat_map(|line| line.windows(2))
                .map(|seg| distance_to_segment(p, seg[0], seg[1]))
                .fold(f32::MAX, f32::min);
            let ink_cov = (half + 0.5 - glyph).clamp(0.0, 1.0);
            if ink_cov > 0.0 {
                blend(img.get_pixel_mut(x, y), ink, ink_cov);
            }
        }
    }
}

/// Alpha-blend `color` over `px` with the given coverage (0..=1).
fn blend(px: &mut Rgba<u8>, color: [u8; 4], coverage: f32) {
    let coverage = coverage * (color[3] as f32 / 255.0);
    if coverage <= 0.0 {
        return;
    }
    for (channel, target) in px.0.iter_mut().zip(color).take(3) {
        let blended = *channel as f32 * (1.0 - coverage) + target as f32 * coverage;
        *channel = blended.round().clamp(0.0, 255.0) as u8;
    }
    let alpha = px.0[3] as f32 + (255.0 - px.0[3] as f32) * coverage;
    px.0[3] = alpha.round().clamp(0.0, 255.0) as u8;
}

/// Minimum blackout side and arrow length, in screenshot pixels, for a drag
/// to count as a new annotation rather than a stray click.
pub const MIN_CREATE_SIZE: f32 = 4.0;

/// An in-flight pointer drag on the annotation canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragOp {
    /// Drawing a new annotation; `index` is its slot in the document.
    Create { index: usize, start: Pt },
    /// Moving an annotation by its body.
    Move {
        index: usize,
        start: Pt,
        original: Annotation,
    },
    /// Resizing a blackout from one of its handles.
    Resize {
        index: usize,
        handle: Handle,
        start: Pt,
        original: Rect,
    },
    /// Dragging one end of an arrow.
    MoveEnd { index: usize, end: ArrowEnd },
}

/// The annotations on one screenshot, with selection and undo history.
#[derive(Clone, Debug)]
pub struct AnnotationDoc {
    items: Vec<Annotation>,
    selected: Option<usize>,
    undo: Vec<Vec<Annotation>>,
    redo: Vec<Vec<Annotation>>,
    /// Snapshot taken when a drag begins; committed to `undo` at drag end
    /// only if the drag actually changed something.
    drag_snapshot: Option<Vec<Annotation>>,
    width: f32,
    height: f32,
}

/// Undo depth cap — plenty for a feedback screenshot.
const MAX_UNDO: usize = 100;

impl AnnotationDoc {
    /// An empty document over an image of `width × height` pixels.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            items: Vec::new(),
            selected: None,
            undo: Vec::new(),
            redo: Vec::new(),
            drag_snapshot: None,
            width: width as f32,
            height: height as f32,
        }
    }

    pub fn items(&self) -> &[Annotation] {
        &self.items
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn select(&mut self, index: Option<usize>) {
        self.selected = index.filter(|&i| i < self.items.len());
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn clamp_point(&self, p: Pt) -> Pt {
        Pt::new(p.x.clamp(0.0, self.width), p.y.clamp(0.0, self.height))
    }

    fn push_undo(&mut self, snapshot: Vec<Annotation>) {
        self.undo.push(snapshot);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Add an annotation as its own undo step and select it. (The dialog
    /// adds by drawing — see [`AnnotationDoc::begin_drag`].)
    #[cfg(test)]
    pub fn add(&mut self, annotation: Annotation) -> usize {
        self.push_undo(self.items.clone());
        self.items.push(annotation);
        let index = self.items.len() - 1;
        self.selected = Some(index);
        index
    }

    /// Delete the selected annotation. Returns whether anything was removed.
    pub fn delete_selected(&mut self) -> bool {
        let Some(index) = self.selected.filter(|&i| i < self.items.len()) else {
            return false;
        };
        self.push_undo(self.items.clone());
        self.items.remove(index);
        self.selected = None;
        true
    }

    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo.push(std::mem::replace(&mut self.items, previous));
        self.selected = None;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.items, next));
        self.selected = None;
        true
    }

    /// What `p` lands on, topmost first. The selected annotation's handles /
    /// arrow ends win over everything (they are drawn on top); then bodies
    /// from the last-drawn down. `tolerance` is the grab slop in pixels (also
    /// the handle size).
    pub fn hit_test(&self, p: Pt, tolerance: f32) -> Option<Hit> {
        if let Some(index) = self.selected
            && let Some(item) = self.items.get(index)
        {
            match item {
                Annotation::Blackout(rect) => {
                    for handle in Handle::ALL {
                        if handle_rect(rect, handle, tolerance * 2.0).contains(p) {
                            return Some(Hit::Handle(index, handle));
                        }
                    }
                }
                Annotation::Arrow { from, to } => {
                    if p.distance(*to) <= tolerance {
                        return Some(Hit::ArrowEnd(index, ArrowEnd::Head));
                    }
                    if p.distance(*from) <= tolerance {
                        return Some(Hit::ArrowEnd(index, ArrowEnd::Tail));
                    }
                }
            }
        }
        self.items
            .iter()
            .enumerate()
            .rev()
            .find(|(_, item)| match item {
                Annotation::Blackout(rect) => rect.contains(p),
                Annotation::Arrow { from, to } => distance_to_segment(p, *from, *to) <= tolerance,
            })
            .map(|(index, _)| Hit::Body(index))
    }

    /// Start a pointer drag at `p` with `tool` active. Grabbing an existing
    /// annotation (handle, arrow end, or body) manipulates it with any tool;
    /// on empty space the Arrow / Blackout tools start drawing a new one and
    /// the Select tool just clears the selection (returning `None`).
    pub fn begin_drag(&mut self, tool: Tool, p: Pt, tolerance: f32) -> Option<DragOp> {
        let p = self.clamp_point(p);
        let op = match self.hit_test(p, tolerance) {
            Some(Hit::Handle(index, handle)) => match self.items[index] {
                Annotation::Blackout(rect) => Some(DragOp::Resize {
                    index,
                    handle,
                    start: p,
                    original: rect,
                }),
                Annotation::Arrow { .. } => None,
            },
            Some(Hit::ArrowEnd(index, end)) => Some(DragOp::MoveEnd { index, end }),
            Some(Hit::Body(index)) => {
                self.selected = Some(index);
                Some(DragOp::Move {
                    index,
                    start: p,
                    original: self.items[index],
                })
            }
            None => match tool {
                Tool::Select => {
                    self.selected = None;
                    None
                }
                Tool::Arrow | Tool::Blackout => {
                    let annotation = if tool == Tool::Arrow {
                        Annotation::Arrow { from: p, to: p }
                    } else {
                        Annotation::Blackout(Rect::new(p.x, p.y, 0.0, 0.0))
                    };
                    self.drag_snapshot = Some(self.items.clone());
                    self.items.push(annotation);
                    let index = self.items.len() - 1;
                    self.selected = Some(index);
                    return Some(DragOp::Create { index, start: p });
                }
            },
        };
        if op.is_some() {
            self.drag_snapshot = Some(self.items.clone());
        }
        op
    }

    /// Apply the pointer's new position `p` to the drag.
    pub fn update_drag(&mut self, op: &DragOp, p: Pt) {
        let p = self.clamp_point(p);
        let (w, h) = (self.width, self.height);
        match *op {
            DragOp::Create { index, start } => {
                if let Some(item) = self.items.get_mut(index) {
                    *item = match item {
                        // The press point is what the user is pointing at, so
                        // the head stays there and the tail (with its number
                        // badge) follows the pointer.
                        Annotation::Arrow { .. } => Annotation::Arrow { from: p, to: start },
                        Annotation::Blackout(_) => {
                            Annotation::Blackout(Rect::from_corners(start, p))
                        }
                    };
                }
            }
            DragOp::Move {
                index,
                start,
                original,
            } => {
                let (dx, dy) = (p.x - start.x, p.y - start.y);
                if let Some(item) = self.items.get_mut(index) {
                    *item = match original {
                        Annotation::Blackout(rect) => {
                            Annotation::Blackout(rect.translated(dx, dy).kept_inside(w, h))
                        }
                        Annotation::Arrow { from, to } => {
                            // Keep both ends on the image: limit the shift to
                            // what the arrow's bounding box allows.
                            let min_x = from.x.min(to.x);
                            let max_x = from.x.max(to.x);
                            let min_y = from.y.min(to.y);
                            let max_y = from.y.max(to.y);
                            let dx = dx.clamp(-min_x, w - max_x);
                            let dy = dy.clamp(-min_y, h - max_y);
                            Annotation::Arrow {
                                from: from.offset(dx, dy),
                                to: to.offset(dx, dy),
                            }
                        }
                    };
                }
            }
            DragOp::Resize {
                index,
                handle,
                start,
                original,
            } => {
                if let Some(item) = self.items.get_mut(index) {
                    *item = Annotation::Blackout(resize_rect(
                        original,
                        handle,
                        p.x - start.x,
                        p.y - start.y,
                        MIN_CREATE_SIZE,
                        (w, h),
                    ));
                }
            }
            DragOp::MoveEnd { index, end } => {
                if let Some(Annotation::Arrow { from, to }) = self.items.get_mut(index) {
                    match end {
                        ArrowEnd::Tail => *from = p,
                        ArrowEnd::Head => *to = p,
                    }
                }
            }
        }
    }

    /// Finish the drag: a newly drawn annotation too small to be deliberate
    /// is discarded, and a drag that changed anything becomes one undo step.
    pub fn end_drag(&mut self, op: &DragOp) {
        if let DragOp::Create { index, .. } = *op {
            let degenerate = match self.items.get(index) {
                Some(Annotation::Arrow { from, to }) => from.distance(*to) < MIN_CREATE_SIZE,
                Some(Annotation::Blackout(rect)) => {
                    rect.w < MIN_CREATE_SIZE || rect.h < MIN_CREATE_SIZE
                }
                None => false,
            };
            if degenerate {
                self.items.remove(index);
                self.selected = None;
            }
        }
        if let Some(snapshot) = self.drag_snapshot.take()
            && snapshot != self.items
        {
            self.push_undo(snapshot);
        }
    }
}

/// Arrow color (a vivid red that reads on both light and dark themes).
pub const ARROW_RGBA: [u8; 4] = [0xE5, 0x2B, 0x2B, 0xFF];

/// The integer pixel span a blackout covers, `[x0, x1) × [y0, y1)`: every
/// pixel it touches even partially (floor / ceil), clamped to the image.
/// `None` when the rectangle misses the image entirely.
pub fn blackout_pixel_span(rect: &Rect, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    let x0 = rect.x.floor().max(0.0) as u32;
    let y0 = rect.y.floor().max(0.0) as u32;
    let x1 = (rect.right().ceil().max(0.0) as u32).min(width);
    let y1 = (rect.bottom().ceil().max(0.0) as u32).min(height);
    (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
}

/// Burn `items` into a copy of `base` at its native resolution: arrows
/// first, then their numbered badges, then every blackout as opaque black on
/// top, so nothing — arrow, badge or original pixel — shows through a
/// blackout.
pub fn flatten(base: &RgbaImage, items: &[Annotation], arrow_width: f32) -> RgbaImage {
    let mut out = base.clone();
    for item in items {
        if let Annotation::Arrow { from, to } = item {
            draw_arrow(&mut out, *from, *to, arrow_width, ARROW_RGBA);
        }
    }
    let (w, h) = (out.width() as f32, out.height() as f32);
    for (item, number) in items.iter().zip(arrow_numbers(items)) {
        if let (Annotation::Arrow { from, .. }, Some(number)) = (item, number) {
            let badge = badge_geometry(*from, number, arrow_width, w, h);
            draw_badge(&mut out, &badge, ARROW_RGBA, BADGE_INK_RGBA);
        }
    }
    for item in items {
        if let Annotation::Blackout(rect) = item
            && let Some((x0, y0, x1, y1)) = blackout_pixel_span(rect, out.width(), out.height())
        {
            for y in y0..y1 {
                for x in x0..x1 {
                    out.put_pixel(x, y, Rgba([0, 0, 0, 255]));
                }
            }
        }
    }
    out
}

/// Signed-area side test for point-in-triangle and edge distance.
fn point_in_triangle(p: Pt, tri: &[Pt; 3]) -> bool {
    let sign = |a: Pt, b: Pt, c: Pt| (a.x - c.x) * (b.y - c.y) - (b.x - c.x) * (a.y - c.y);
    let d1 = sign(p, tri[0], tri[1]);
    let d2 = sign(p, tri[1], tri[2]);
    let d3 = sign(p, tri[2], tri[0]);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

/// Rasterize an anti-aliased arrow into `img`, alpha-blending `color` over
/// the existing pixels.
pub fn draw_arrow(img: &mut RgbaImage, from: Pt, to: Pt, width: f32, color: [u8; 4]) {
    let Some(geometry) = arrow_geometry(from, to, width) else {
        return;
    };
    let (w, h) = (img.width() as f32, img.height() as f32);
    let pad = width + 2.0;
    let xs = [from.x, to.x, geometry.head[1].x, geometry.head[2].x];
    let ys = [from.y, to.y, geometry.head[1].y, geometry.head[2].y];
    let min_x = (xs.iter().copied().fold(f32::MAX, f32::min) - pad).max(0.0);
    let max_x = (xs.iter().copied().fold(f32::MIN, f32::max) + pad).min(w - 1.0);
    let min_y = (ys.iter().copied().fold(f32::MAX, f32::min) - pad).max(0.0);
    let max_y = (ys.iter().copied().fold(f32::MIN, f32::max) + pad).min(h - 1.0);
    if min_x > max_x || min_y > max_y {
        return;
    }
    let half = width / 2.0;
    let head = geometry.head;
    let head_edges = [(head[0], head[1]), (head[1], head[2]), (head[2], head[0])];
    for y in (min_y as u32)..=(max_y as u32) {
        for x in (min_x as u32)..=(max_x as u32) {
            // Sample at the pixel center.
            let p = Pt::new(x as f32 + 0.5, y as f32 + 0.5);
            let shaft = (half + 0.5 - distance_to_segment(p, geometry.shaft.0, geometry.shaft.1))
                .clamp(0.0, 1.0);
            let head_cov = if point_in_triangle(p, &head) {
                1.0
            } else {
                let edge = head_edges
                    .iter()
                    .map(|(a, b)| distance_to_segment(p, *a, *b))
                    .fold(f32::MAX, f32::min);
                (0.5 - edge).clamp(0.0, 1.0)
            };
            blend(img.get_pixel_mut(x, y), color, shaft.max(head_cov));
        }
    }
}

/// Encode an RGBA image as PNG.
pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, String> {
    use image::ImageEncoder as _;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILL: Rgba<u8> = Rgba([200, 150, 100, 255]);

    fn filled(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, FILL)
    }

    #[test]
    fn blackout_zeroes_every_touched_pixel_and_nothing_else() {
        let base = filled(20, 20);
        let rect = Rect::new(2.5, 3.25, 4.0, 5.5); // spans x 2..7, y 3..9
        let out = flatten(&base, &[Annotation::Blackout(rect)], 3.0);
        for y in 0..20 {
            for x in 0..20 {
                let inside = (2..7).contains(&x) && (3..9).contains(&y);
                let expected = if inside { Rgba([0, 0, 0, 255]) } else { FILL };
                assert_eq!(*out.get_pixel(x, y), expected, "pixel ({x},{y})");
            }
        }
    }

    #[test]
    fn blackout_wins_over_an_arrow_crossing_it() {
        let base = filled(60, 40);
        let rect = Rect::new(20.0, 10.0, 20.0, 20.0);
        let items = [
            // The blackout is listed FIRST but must still paint on top.
            Annotation::Blackout(rect),
            Annotation::Arrow {
                from: Pt::new(2.0, 20.0),
                to: Pt::new(58.0, 20.0),
            },
        ];
        let out = flatten(&base, &items, 6.0);
        for y in 10..30 {
            for x in 20..40 {
                assert_eq!(*out.get_pixel(x, y), Rgba([0, 0, 0, 255]), "({x},{y})");
            }
        }
        // The arrow still shows outside the blackout.
        assert_eq!(out.get_pixel(10, 20).0[..3], ARROW_RGBA[..3]);
    }

    #[test]
    fn blackout_partially_off_image_is_clamped() {
        let base = filled(10, 10);
        let out = flatten(
            &base,
            &[Annotation::Blackout(Rect::new(-5.0, 7.0, 8.0, 20.0))],
            3.0,
        );
        assert_eq!(*out.get_pixel(0, 9), Rgba([0, 0, 0, 255]));
        assert_eq!(*out.get_pixel(2, 7), Rgba([0, 0, 0, 255]));
        assert_eq!(*out.get_pixel(3, 7), FILL);
        assert_eq!(*out.get_pixel(0, 6), FILL);
        assert!(blackout_pixel_span(&Rect::new(50.0, 50.0, 5.0, 5.0), 10, 10).is_none());
    }

    #[test]
    fn arrow_is_drawn_along_the_shaft_and_at_the_head() {
        let base = filled(100, 60);
        let from = Pt::new(10.0, 30.0);
        let to = Pt::new(90.0, 30.0);
        let out = flatten(&base, &[Annotation::Arrow { from, to }], 4.0);
        // Middle of the shaft is solid arrow color.
        assert_eq!(out.get_pixel(40, 30).0, ARROW_RGBA);
        // Inside the head, wider than the shaft.
        let g = arrow_geometry(from, to, 4.0).unwrap();
        let head_mid_x = ((g.head[0].x + g.head[1].x) / 2.0) as u32;
        assert_eq!(out.get_pixel(head_mid_x, 31).0, ARROW_RGBA);
        assert_eq!(out.get_pixel(head_mid_x - 2, 34).0, ARROW_RGBA);
        // Well away from the arrow is untouched.
        assert_eq!(*out.get_pixel(40, 5), FILL);
        assert_eq!(*out.get_pixel(40, 50), FILL);
        // Beyond the tip is untouched.
        assert_eq!(*out.get_pixel(95, 30), FILL);
        // The base image is not modified in place.
        assert_eq!(*base.get_pixel(40, 30), FILL);
    }

    #[test]
    fn arrows_are_numbered_in_document_order_skipping_blackouts() {
        let arrow = |x: f32| Annotation::Arrow {
            from: Pt::new(x, 0.0),
            to: Pt::new(x, 10.0),
        };
        let items = [
            arrow(1.0),
            Annotation::Blackout(Rect::new(0.0, 0.0, 5.0, 5.0)),
            arrow(2.0),
            arrow(3.0),
        ];
        assert_eq!(arrow_numbers(&items), vec![Some(1), None, Some(2), Some(3)]);
    }

    #[test]
    fn numbered_badge_is_burned_in_at_the_arrow_tail() {
        let base = filled(200, 120);
        let from = Pt::new(60.0, 60.0);
        let to = Pt::new(180.0, 60.0);
        let out = flatten(&base, &[Annotation::Arrow { from, to }], 6.0);
        let badge = badge_geometry(from, 1, 6.0, 200.0, 120.0);
        assert_eq!(badge.center, from);
        // The disc is arrow red near its rim (away from the digit)…
        let rim = Pt::new(from.x, from.y + badge.radius - 2.0);
        assert_eq!(out.get_pixel(rim.x as u32, rim.y as u32).0, ARROW_RGBA);
        // …and the digit's stroke is white ink.
        let stroke_mid = badge.strokes[0][2];
        let ink = out
            .get_pixel(stroke_mid.x as u32, stroke_mid.y as u32 - 2)
            .0;
        assert_eq!(ink, BADGE_INK_RGBA, "digit ink at {stroke_mid:?}");
        // Outside the disc and off the arrow is untouched.
        assert_eq!(*out.get_pixel(60, 20), FILL);
    }

    #[test]
    fn badges_stay_inside_the_image_and_widen_for_two_digits() {
        let corner = badge_geometry(Pt::new(0.0, 0.0), 3, 6.0, 300.0, 200.0);
        assert!((corner.center.x - corner.radius).abs() < 1e-3);
        assert!((corner.center.y - corner.radius).abs() < 1e-3);
        let far = badge_geometry(Pt::new(400.0, 400.0), 3, 6.0, 300.0, 200.0);
        assert!((far.center.x + far.radius - 300.0).abs() < 1e-3);
        assert!((far.center.y + far.radius - 200.0).abs() < 1e-3);
        let one = badge_geometry(Pt::new(100.0, 100.0), 7, 6.0, 300.0, 200.0);
        let two = badge_geometry(Pt::new(100.0, 100.0), 12, 6.0, 300.0, 200.0);
        assert!(two.radius > one.radius);
        // Every digit stroke point lies inside its disc.
        for badge in [&one, &two] {
            for p in badge.strokes.iter().flatten() {
                assert!(p.distance(badge.center) < badge.radius, "{p:?}");
            }
        }
    }

    #[test]
    fn blackout_hides_a_badge_under_it() {
        let base = filled(200, 120);
        let from = Pt::new(60.0, 60.0);
        let items = [
            Annotation::Arrow {
                from,
                to: Pt::new(180.0, 60.0),
            },
            Annotation::Blackout(Rect::new(30.0, 30.0, 60.0, 60.0)),
        ];
        let out = flatten(&base, &items, 6.0);
        for y in 30..90 {
            for x in 30..90 {
                assert_eq!(*out.get_pixel(x, y), Rgba([0, 0, 0, 255]), "({x},{y})");
            }
        }
    }

    #[test]
    fn arrow_geometry_head_points_at_the_target() {
        let g = arrow_geometry(Pt::new(0.0, 0.0), Pt::new(0.0, 100.0), 4.0).unwrap();
        assert_eq!(g.head[0], Pt::new(0.0, 100.0));
        // Head base sits 5 × width before the tip, symmetric about the axis,
        // and spans 1.2 × its length.
        assert!((g.head[1].y - 80.0).abs() < 1e-3);
        assert!((g.head[1].x + g.head[2].x).abs() < 1e-3);
        assert!((g.head[1].x - g.head[2].x).abs() - 24.0 < 1e-3);
        assert!(g.shaft.1.y < 100.0 && g.shaft.1.y > 80.0);
        assert!(arrow_geometry(Pt::new(5.0, 5.0), Pt::new(5.0, 5.0), 4.0).is_none());
        assert_eq!(arrow_width_for_scale(2.0), 9.0);
        assert_eq!(arrow_width_for_scale(1.0), 5.0);
    }

    #[test]
    fn handle_centers_sit_on_corners_and_edge_midpoints() {
        let r = Rect::new(10.0, 20.0, 100.0, 40.0);
        assert_eq!(handle_center(&r, Handle::TopLeft), Pt::new(10.0, 20.0));
        assert_eq!(handle_center(&r, Handle::Top), Pt::new(60.0, 20.0));
        assert_eq!(handle_center(&r, Handle::TopRight), Pt::new(110.0, 20.0));
        assert_eq!(handle_center(&r, Handle::Right), Pt::new(110.0, 40.0));
        assert_eq!(handle_center(&r, Handle::BottomRight), Pt::new(110.0, 60.0));
        assert_eq!(handle_center(&r, Handle::Bottom), Pt::new(60.0, 60.0));
        assert_eq!(handle_center(&r, Handle::BottomLeft), Pt::new(10.0, 60.0));
        assert_eq!(handle_center(&r, Handle::Left), Pt::new(10.0, 40.0));
        let hr = handle_rect(&r, Handle::BottomRight, 8.0);
        assert_eq!(hr, Rect::new(106.0, 56.0, 8.0, 8.0));
    }

    #[test]
    fn resize_moves_only_the_grabbed_edges() {
        let r = Rect::new(10.0, 10.0, 50.0, 30.0);
        let bounds = (200.0, 200.0);
        assert_eq!(
            resize_rect(r, Handle::Right, 15.0, 99.0, 4.0, bounds),
            Rect::new(10.0, 10.0, 65.0, 30.0)
        );
        assert_eq!(
            resize_rect(r, Handle::Top, 99.0, -5.0, 4.0, bounds),
            Rect::new(10.0, 5.0, 50.0, 35.0)
        );
        assert_eq!(
            resize_rect(r, Handle::TopLeft, 5.0, 5.0, 4.0, bounds),
            Rect::new(15.0, 15.0, 45.0, 25.0)
        );
        assert_eq!(
            resize_rect(r, Handle::BottomLeft, -10.0, 10.0, 4.0, bounds),
            Rect::new(0.0, 10.0, 60.0, 40.0)
        );
    }

    #[test]
    fn resize_never_inverts_collapses_or_leaves_the_image() {
        let r = Rect::new(10.0, 10.0, 50.0, 30.0);
        let bounds = (100.0, 60.0);
        // Dragging the left edge past the right one stops `min` short.
        let shrunk = resize_rect(r, Handle::Left, 500.0, 0.0, 4.0, bounds);
        assert_eq!(shrunk, Rect::new(56.0, 10.0, 4.0, 30.0));
        // Dragging the bottom-right corner off the image clamps to it.
        let grown = resize_rect(r, Handle::BottomRight, 500.0, 500.0, 4.0, bounds);
        assert_eq!(grown, Rect::new(10.0, 10.0, 90.0, 50.0));
        // Dragging the top-left corner off the image clamps to the origin.
        let origin = resize_rect(r, Handle::TopLeft, -50.0, -50.0, 4.0, bounds);
        assert_eq!(origin, Rect::new(0.0, 0.0, 60.0, 40.0));
    }

    #[test]
    fn hit_test_prefers_selected_handles_then_topmost_body() {
        let mut doc = AnnotationDoc::new(200, 200);
        doc.add(Annotation::Blackout(Rect::new(10.0, 10.0, 50.0, 50.0)));
        doc.add(Annotation::Blackout(Rect::new(40.0, 40.0, 50.0, 50.0)));
        // Overlap goes to the later (topmost) box.
        assert_eq!(doc.hit_test(Pt::new(45.0, 45.0), 4.0), Some(Hit::Body(1)));
        // Select the first: its bottom-right handle (inside box 1) wins.
        doc.select(Some(0));
        assert_eq!(
            doc.hit_test(Pt::new(61.0, 59.0), 4.0),
            Some(Hit::Handle(0, Handle::BottomRight))
        );
        assert_eq!(doc.hit_test(Pt::new(150.0, 150.0), 4.0), None);
        // Arrows hit within tolerance of the segment; ends when selected.
        let a = doc.add(Annotation::Arrow {
            from: Pt::new(100.0, 150.0),
            to: Pt::new(180.0, 150.0),
        });
        assert_eq!(doc.hit_test(Pt::new(140.0, 153.0), 4.0), Some(Hit::Body(a)));
        assert_eq!(
            doc.hit_test(Pt::new(179.0, 151.0), 4.0),
            Some(Hit::ArrowEnd(a, ArrowEnd::Head))
        );
        assert_eq!(
            doc.hit_test(Pt::new(101.0, 150.0), 4.0),
            Some(Hit::ArrowEnd(a, ArrowEnd::Tail))
        );
    }

    #[test]
    fn drawing_a_blackout_then_undo_and_redo() {
        let mut doc = AnnotationDoc::new(300, 200);
        let op = doc
            .begin_drag(Tool::Blackout, Pt::new(50.0, 40.0), 4.0)
            .unwrap();
        doc.update_drag(&op, Pt::new(20.0, 90.0));
        doc.end_drag(&op);
        assert_eq!(
            doc.items(),
            &[Annotation::Blackout(Rect::new(20.0, 40.0, 30.0, 50.0))]
        );
        assert_eq!(doc.selected(), Some(0));
        assert!(doc.undo());
        assert!(doc.items().is_empty());
        assert!(doc.redo());
        assert_eq!(doc.items().len(), 1);
    }

    /// Drawing an arrow: the head lands where the press happened (the thing
    /// being pointed at) and the tail, with its number badge, follows the drag.
    #[test]
    fn a_drawn_arrow_points_at_the_press_point() {
        let mut doc = AnnotationDoc::new(300, 200);
        let op = doc
            .begin_drag(Tool::Arrow, Pt::new(50.0, 40.0), 4.0)
            .unwrap();
        doc.update_drag(&op, Pt::new(150.0, 120.0));
        doc.end_drag(&op);
        assert_eq!(
            doc.items(),
            &[Annotation::Arrow {
                from: Pt::new(150.0, 120.0),
                to: Pt::new(50.0, 40.0),
            }]
        );
    }

    #[test]
    fn a_stray_click_with_a_drawing_tool_adds_nothing() {
        let mut doc = AnnotationDoc::new(300, 200);
        let op = doc
            .begin_drag(Tool::Arrow, Pt::new(50.0, 40.0), 4.0)
            .unwrap();
        doc.update_drag(&op, Pt::new(51.0, 41.0));
        doc.end_drag(&op);
        assert!(doc.items().is_empty());
        assert!(!doc.can_undo(), "a discarded draw is not an undo step");
        // Select tool on empty space starts nothing.
        assert!(
            doc.begin_drag(Tool::Select, Pt::new(5.0, 5.0), 4.0)
                .is_none()
        );
    }

    #[test]
    fn moving_resizing_and_deleting_are_undoable() {
        let mut doc = AnnotationDoc::new(300, 200);
        doc.add(Annotation::Blackout(Rect::new(10.0, 10.0, 40.0, 40.0)));
        // Move by the body (with any tool), clamped inside the image.
        let op = doc
            .begin_drag(Tool::Arrow, Pt::new(20.0, 20.0), 4.0)
            .unwrap();
        assert!(matches!(op, DragOp::Move { .. }));
        doc.update_drag(&op, Pt::new(-100.0, 30.0));
        doc.end_drag(&op);
        assert_eq!(
            doc.items()[0],
            Annotation::Blackout(Rect::new(0.0, 20.0, 40.0, 40.0))
        );
        // Resize by the bottom-right handle.
        let op = doc
            .begin_drag(Tool::Select, Pt::new(40.0, 60.0), 4.0)
            .unwrap();
        assert!(matches!(
            op,
            DragOp::Resize {
                handle: Handle::BottomRight,
                ..
            }
        ));
        doc.update_drag(&op, Pt::new(70.0, 80.0));
        doc.end_drag(&op);
        assert_eq!(
            doc.items()[0],
            Annotation::Blackout(Rect::new(0.0, 20.0, 70.0, 60.0))
        );
        assert!(doc.delete_selected());
        assert!(doc.items().is_empty());
        // Undo walks back delete → resize → move → add.
        assert!(doc.undo());
        assert_eq!(
            doc.items()[0],
            Annotation::Blackout(Rect::new(0.0, 20.0, 70.0, 60.0))
        );
        assert!(doc.undo());
        assert!(doc.undo());
        assert_eq!(
            doc.items()[0],
            Annotation::Blackout(Rect::new(10.0, 10.0, 40.0, 40.0))
        );
        assert!(doc.undo());
        assert!(doc.items().is_empty());
        assert!(!doc.undo());
    }

    #[test]
    fn dragging_an_arrow_end_moves_only_that_end() {
        let mut doc = AnnotationDoc::new(300, 200);
        doc.add(Annotation::Arrow {
            from: Pt::new(10.0, 10.0),
            to: Pt::new(100.0, 100.0),
        });
        doc.add(Annotation::Blackout(Rect::new(200.0, 10.0, 20.0, 20.0)));
        doc.select(Some(0));
        let op = doc
            .begin_drag(Tool::Select, Pt::new(100.0, 100.0), 4.0)
            .unwrap();
        assert_eq!(
            op,
            DragOp::MoveEnd {
                index: 0,
                end: ArrowEnd::Head
            }
        );
        doc.update_drag(&op, Pt::new(150.0, 400.0)); // clamped to the image
        doc.end_drag(&op);
        assert_eq!(
            doc.items()[0],
            Annotation::Arrow {
                from: Pt::new(10.0, 10.0),
                to: Pt::new(150.0, 200.0),
            }
        );
    }

    #[test]
    fn png_roundtrips_at_native_size() {
        let img = filled(33, 17);
        let png = encode_png(&img).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (33, 17));
        assert_eq!(*decoded.get_pixel(5, 5), FILL);
    }
}
