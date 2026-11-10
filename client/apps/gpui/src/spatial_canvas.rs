//! Canvas sketch of a parsed [`Geometry`]: points, lines, and polygons on a
//! normalized 2D view. Theme tokens only — no GIS basemap.

use std::sync::Arc;

use gpui::{Bounds, IntoElement, Pixels, Point, canvas, fill, point, prelude::*, px, size};
use sqlly_core::spatial::{Coord, Geometry, Shape};

use crate::theme::Theme;

const INSET: f32 = 12.0;
const POINT_SIZE: f32 = 6.0;
const MAX_PAINT_POINTS: usize = 50_000;

pub fn spatial_canvas(geometry: Arc<Geometry>) -> impl IntoElement {
    canvas(
        |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            window.paint_quad(fill(bounds, Theme::panel_bg()));
            let total = geometry.point_count();
            if total == 0 {
                return;
            }
            let stride = stride_for(total);
            let bbox = padded_bbox(&geometry);
            let mapper = Mapper::new(bounds, bbox);
            for shape in &geometry.shapes {
                match shape {
                    Shape::Point(c) => {
                        let p = mapper.map(*c);
                        fill_quad(
                            window,
                            f32::from(p.x) - POINT_SIZE / 2.0,
                            f32::from(p.y) - POINT_SIZE / 2.0,
                            POINT_SIZE,
                            POINT_SIZE,
                            Theme::accent(),
                        );
                    }
                    Shape::LineString(pts) => {
                        paint_line(window, &mapper, pts, stride, px(1.5));
                    }
                    Shape::Polygon(rings) => {
                        if let Some(exterior) = rings.first() {
                            paint_polygon_fill(window, &mapper, exterior, stride);
                        }
                        for ring in rings {
                            paint_line(window, &mapper, ring, stride, px(1.0));
                        }
                    }
                }
            }
        },
    )
    .size_full()
}

fn stride_for(total: usize) -> usize {
    if total <= MAX_PAINT_POINTS {
        1
    } else {
        total.div_ceil(MAX_PAINT_POINTS).max(1)
    }
}

fn padded_bbox(geometry: &Geometry) -> (f64, f64, f64, f64) {
    let (min_x, min_y, max_x, max_y) = match geometry.bbox() {
        Some(b) => (b.min_x, b.min_y, b.max_x, b.max_y),
        None => (-1.0, -1.0, 1.0, 1.0),
    };
    let mut w = max_x - min_x;
    let mut h = max_y - min_y;
    if !w.is_finite() || w <= 0.0 {
        w = 1.0;
    }
    if !h.is_finite() || h <= 0.0 {
        h = 1.0;
    }
    let pad = w.max(h) * 0.08;
    // Degenerate (point or flat) geometries get a square so the sketch has room.
    let side = (w + pad * 2.0).max(h + pad * 2.0);
    let cx = (min_x + max_x) / 2.0;
    let cy = (min_y + max_y) / 2.0;
    (
        cx - side / 2.0,
        cy - side / 2.0,
        cx + side / 2.0,
        cy + side / 2.0,
    )
}

struct Mapper {
    origin: Point<Pixels>,
    scale: f32,
    min_x: f64,
    min_y: f64,
    height: f32,
}

impl Mapper {
    fn new(bounds: Bounds<Pixels>, bbox: (f64, f64, f64, f64)) -> Self {
        let (min_x, min_y, max_x, max_y) = bbox;
        let inner_w = (f32::from(bounds.size.width) - INSET * 2.0).max(1.0);
        let inner_h = (f32::from(bounds.size.height) - INSET * 2.0).max(1.0);
        let gw = (max_x - min_x).max(f64::EPSILON) as f32;
        let gh = (max_y - min_y).max(f64::EPSILON) as f32;
        let scale = (inner_w / gw).min(inner_h / gh);
        let used_w = gw * scale;
        let used_h = gh * scale;
        let ox = f32::from(bounds.origin.x);
        let oy = f32::from(bounds.origin.y);
        Self {
            origin: point(
                px(ox + INSET + (inner_w - used_w) / 2.0),
                px(oy + INSET + (inner_h - used_h) / 2.0),
            ),
            scale,
            min_x,
            min_y,
            height: used_h,
        }
    }

    fn map(&self, c: Coord) -> Point<Pixels> {
        let x = ((c.x - self.min_x) as f32) * self.scale;
        let y = self.height - ((c.y - self.min_y) as f32) * self.scale;
        let ox = f32::from(self.origin.x);
        let oy = f32::from(self.origin.y);
        point(px(ox + x), px(oy + y))
    }
}

fn sampled<'a>(pts: &'a [Coord], stride: usize) -> impl Iterator<Item = Coord> + 'a {
    pts.iter().step_by(stride.max(1)).copied().chain(
        pts.last()
            .copied()
            .filter(|_| stride > 1 && !pts.is_empty() && !(pts.len() - 1).is_multiple_of(stride)),
    )
}

fn paint_line(
    window: &mut gpui::Window,
    mapper: &Mapper,
    pts: &[Coord],
    stride: usize,
    width: Pixels,
) {
    let mut iter = sampled(pts, stride);
    let Some(first) = iter.next() else {
        return;
    };
    let mut path = gpui::PathBuilder::stroke(width);
    let p = mapper.map(first);
    path.move_to(p);
    let mut more = false;
    for c in iter {
        path.line_to(mapper.map(c));
        more = true;
    }
    if !more {
        return;
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, Theme::accent());
    }
}

fn paint_polygon_fill(window: &mut gpui::Window, mapper: &Mapper, ring: &[Coord], stride: usize) {
    let mut iter = sampled(ring, stride);
    let Some(first) = iter.next() else {
        return;
    };
    let mut area = gpui::PathBuilder::fill();
    area.move_to(mapper.map(first));
    let mut more = false;
    for c in iter {
        area.line_to(mapper.map(c));
        more = true;
    }
    if !more {
        return;
    }
    area.close();
    if let Ok(path) = area.build() {
        window.paint_path(path, Theme::accent().opacity(0.18));
    }
}

fn fill_quad(window: &mut gpui::Window, x: f32, y: f32, w: f32, h: f32, color: gpui::Hsla) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    window.paint_quad(fill(
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        },
        color,
    ));
}
