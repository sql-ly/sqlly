//! A chart as a flat list of drawing primitives. The SVG writer and the
//! PDF painter both consume a [`Scene`], so a chart is the same picture in
//! every export. Coordinates are SVG user units, y pointing down.

use crate::report::text::xml_escape;
use crate::report::theme::Rgb;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fill {
    pub color: Rgb,
    pub opacity: f32,
}

impl Fill {
    pub fn solid(color: Rgb) -> Self {
        Self {
            color,
            opacity: 1.0,
        }
    }

    pub fn alpha(color: Rgb, opacity: f32) -> Self {
        Self { color, opacity }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stroke {
    pub color: Rgb,
    pub width: f32,
    pub dash: Option<(f32, f32)>,
    pub opacity: f32,
}

impl Stroke {
    pub fn new(color: Rgb, width: f32) -> Self {
        Self {
            color,
            width,
            dash: None,
            opacity: 1.0,
        }
    }

    pub fn dashed(mut self, on: f32, off: f32) -> Self {
        self.dash = Some((on, off));
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Start,
    Middle,
    End,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextItem {
    pub x: f32,
    /// Baseline.
    pub y: f32,
    pub text: String,
    pub size: f32,
    pub color: Rgb,
    pub anchor: Anchor,
    pub bold: bool,
    /// Degrees clockwise around (x, y).
    pub rotate: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmd {
    Move(f32, f32),
    Line(f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        fill: Option<Fill>,
        stroke: Option<Stroke>,
        radius: f32,
    },
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        stroke: Stroke,
    },
    Path {
        cmds: Vec<Cmd>,
        fill: Option<Fill>,
        stroke: Option<Stroke>,
    },
    Circle {
        cx: f32,
        cy: f32,
        r: f32,
        fill: Option<Fill>,
        stroke: Option<Stroke>,
    },
    Text(TextItem),
}

/// The area a clickable mark covers, in scene units.
#[derive(Debug, Clone, PartialEq)]
pub enum HitShape {
    Rect { x: f32, y: f32, w: f32, h: f32 },
    Polygon(Vec<(f32, f32)>),
}

impl HitShape {
    fn contains(&self, px: f32, py: f32) -> bool {
        match self {
            HitShape::Rect { x, y, w, h } => px >= *x && px <= x + w && py >= *y && py <= y + h,
            HitShape::Polygon(points) => {
                let mut inside = false;
                let mut j = points.len().wrapping_sub(1);
                for (i, &(xi, yi)) in points.iter().enumerate() {
                    let (xj, yj) = points[j];
                    if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
                        inside = !inside;
                    }
                    j = i;
                }
                inside
            }
        }
    }
}

/// A mark a click can select: its area and the category it stands for,
/// labelled as cross-filters label it (`notebook::cells::spec::category_label`).
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub shape: HitShape,
    pub category: String,
}

/// Thinner marks than this are hit across this width, centered.
const MIN_HIT: f32 = 8.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    pub width: f32,
    pub height: f32,
    pub background: Option<Rgb>,
    /// Accessible name and description.
    pub title: String,
    pub description: String,
    pub items: Vec<Item>,
    /// Clickable marks, drawn order (later ones on top). Not written to SVG.
    pub hits: Vec<Hit>,
}

impl Scene {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            background: None,
            title: String::new(),
            description: String::new(),
            items: Vec::new(),
            hits: Vec::new(),
        }
    }

    /// Make the rectangle a click target for `category` (none for marks
    /// that stand for no single category, such as "Other").
    pub fn hit_rect(&mut self, x: f32, y: f32, w: f32, h: f32, category: Option<&String>) {
        let Some(category) = category else {
            return;
        };
        let (x, w) = if w < MIN_HIT {
            (x + w / 2.0 - MIN_HIT / 2.0, MIN_HIT)
        } else {
            (x, w)
        };
        let (y, h) = if h < MIN_HIT {
            (y + h / 2.0 - MIN_HIT / 2.0, MIN_HIT)
        } else {
            (y, h)
        };
        self.hits.push(Hit {
            shape: HitShape::Rect { x, y, w, h },
            category: category.clone(),
        });
    }

    /// Make the path's outline (cubic curves flattened) a click target.
    pub fn hit_path(&mut self, cmds: &[Cmd], category: Option<&String>) {
        let Some(category) = category else {
            return;
        };
        let mut points: Vec<(f32, f32)> = Vec::new();
        for cmd in cmds {
            match *cmd {
                Cmd::Move(x, y) | Cmd::Line(x, y) => points.push((x, y)),
                Cmd::Cubic(x1, y1, x2, y2, x, y) => {
                    let (x0, y0) = points.last().copied().unwrap_or((x1, y1));
                    for step in 1..=8 {
                        let t = step as f32 / 8.0;
                        let u = 1.0 - t;
                        let b = |p0: f32, p1: f32, p2: f32, p3: f32| {
                            u * u * u * p0
                                + 3.0 * u * u * t * p1
                                + 3.0 * u * t * t * p2
                                + t * t * t * p3
                        };
                        points.push((b(x0, x1, x2, x), b(y0, y1, y2, y)));
                    }
                }
                Cmd::Close => {}
            }
        }
        if points.len() >= 3 {
            self.hits.push(Hit {
                shape: HitShape::Polygon(points),
                category: category.clone(),
            });
        }
    }

    /// The category of the topmost mark at (`x`, `y`), in scene units.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<&str> {
        self.hits
            .iter()
            .rev()
            .find(|hit| hit.shape.contains(x, y))
            .map(|hit| hit.category.as_str())
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, fill: Fill) {
        if w > 0.0 && h > 0.0 {
            self.items.push(Item::Rect {
                x,
                y,
                w,
                h,
                fill: Some(fill),
                stroke: None,
                radius: 0.0,
            });
        }
    }

    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, stroke: Stroke) {
        self.items.push(Item::Line {
            x1,
            y1,
            x2,
            y2,
            stroke,
        });
    }

    pub fn polyline(&mut self, points: &[(f32, f32)], stroke: Stroke) {
        if points.len() < 2 {
            return;
        }
        let mut cmds = vec![Cmd::Move(points[0].0, points[0].1)];
        cmds.extend(points[1..].iter().map(|&(x, y)| Cmd::Line(x, y)));
        self.items.push(Item::Path {
            cmds,
            fill: None,
            stroke: Some(stroke),
        });
    }

    pub fn polygon(&mut self, points: &[(f32, f32)], fill: Fill) {
        if points.len() < 3 {
            return;
        }
        let mut cmds = vec![Cmd::Move(points[0].0, points[0].1)];
        cmds.extend(points[1..].iter().map(|&(x, y)| Cmd::Line(x, y)));
        cmds.push(Cmd::Close);
        self.items.push(Item::Path {
            cmds,
            fill: Some(fill),
            stroke: None,
        });
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, fill: Fill) {
        self.items.push(Item::Circle {
            cx,
            cy,
            r,
            fill: Some(fill),
            stroke: None,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &mut self,
        x: f32,
        y: f32,
        text: impl Into<String>,
        size: f32,
        color: Rgb,
        anchor: Anchor,
    ) {
        self.items.push(Item::Text(TextItem {
            x,
            y,
            text: text.into(),
            size,
            color,
            anchor,
            bold: false,
            rotate: 0.0,
        }));
    }

    pub fn push_text(&mut self, item: TextItem) {
        if !item.text.is_empty() {
            self.items.push(Item::Text(item));
        }
    }
}

/// Cubic Bézier segments for a circular arc from `a0` to `a1` (radians,
/// clockwise from 12 o'clock in screen space), starting with a line or move
/// to the arc's first point as `first` says.
pub fn arc(cx: f32, cy: f32, r: f32, a0: f32, a1: f32, first: Cmd) -> Vec<Cmd> {
    let point = |a: f32| (cx + r * a.sin(), cy - r * a.cos());
    let (sx, sy) = point(a0);
    let mut cmds = vec![match first {
        Cmd::Line(..) => Cmd::Line(sx, sy),
        _ => Cmd::Move(sx, sy),
    }];
    let sweep = a1 - a0;
    let segments = (sweep.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = sweep / segments as f32;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    for i in 0..segments {
        let s = a0 + step * i as f32;
        let e = s + step;
        let (x0, y0) = point(s);
        let (x3, y3) = point(e);
        let (dx0, dy0) = (r * s.cos(), r * s.sin());
        let (dx3, dy3) = (r * e.cos(), r * e.sin());
        cmds.push(Cmd::Cubic(
            x0 + k * dx0,
            y0 + k * dy0,
            x3 - k * dx3,
            y3 - k * dy3,
            x3,
            y3,
        ));
    }
    cmds
}

/// A number for markup: at most two decimals, no trailing zeros, no `-0`.
pub fn num(v: f32) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    let rounded = (v * 100.0).round() / 100.0;
    let mut text = format!("{rounded:.2}");
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if text == "-0" {
        text = "0".into();
    }
    text
}

fn fill_attrs(fill: Option<Fill>) -> String {
    match fill {
        None => " fill=\"none\"".into(),
        Some(f) if f.opacity >= 0.999 => format!(" fill=\"{}\"", f.color.css()),
        Some(f) => format!(
            " fill=\"{}\" fill-opacity=\"{}\"",
            f.color.css(),
            num(f.opacity)
        ),
    }
}

fn stroke_attrs(stroke: Option<Stroke>) -> String {
    let Some(s) = stroke else {
        return String::new();
    };
    let mut out = format!(
        " stroke=\"{}\" stroke-width=\"{}\"",
        s.color.css(),
        num(s.width)
    );
    if let Some((on, off)) = s.dash {
        out.push_str(&format!(" stroke-dasharray=\"{} {}\"", num(on), num(off)));
    }
    if s.opacity < 0.999 {
        out.push_str(&format!(" stroke-opacity=\"{}\"", num(s.opacity)));
    }
    out
}

pub fn path_data(cmds: &[Cmd]) -> String {
    let mut d = String::new();
    for cmd in cmds {
        if !d.is_empty() {
            d.push(' ');
        }
        match *cmd {
            Cmd::Move(x, y) => d.push_str(&format!("M{} {}", num(x), num(y))),
            Cmd::Line(x, y) => d.push_str(&format!("L{} {}", num(x), num(y))),
            Cmd::Cubic(a, b, c, e, x, y) => d.push_str(&format!(
                "C{} {} {} {} {} {}",
                num(a),
                num(b),
                num(c),
                num(e),
                num(x),
                num(y)
            )),
            Cmd::Close => d.push('Z'),
        }
    }
    d
}

pub const FONT_STACK: &str =
    "'Noto Sans', -apple-system, 'Segoe UI', 'Helvetica Neue', Arial, sans-serif";

/// The scene as a standalone SVG document. `id` prefixes the accessible
/// title and description ids so several charts can share one HTML page.
pub fn to_svg(scene: &Scene, id: &str) -> String {
    let id = super::super::text::slug(id);
    let mut out = String::new();
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-labelledby=\"{id}-title {id}-desc\" font-family=\"{font}\">\n",
        w = num(scene.width),
        h = num(scene.height),
        font = xml_escape(FONT_STACK),
    ));
    out.push_str(&format!(
        "<title id=\"{id}-title\">{}</title>\n<desc id=\"{id}-desc\">{}</desc>\n",
        xml_escape(&scene.title),
        xml_escape(&scene.description)
    ));
    if let Some(bg) = scene.background {
        out.push_str(&format!(
            "<rect width=\"{}\" height=\"{}\" fill=\"{}\"/>\n",
            num(scene.width),
            num(scene.height),
            bg.css()
        ));
    }
    for item in &scene.items {
        match item {
            Item::Rect {
                x,
                y,
                w,
                h,
                fill,
                stroke,
                radius,
            } => {
                out.push_str(&format!(
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"",
                    num(*x),
                    num(*y),
                    num(*w),
                    num(*h)
                ));
                if *radius > 0.0 {
                    out.push_str(&format!(" rx=\"{}\"", num(*radius)));
                }
                out.push_str(&fill_attrs(*fill));
                out.push_str(&stroke_attrs(*stroke));
                out.push_str("/>\n");
            }
            Item::Line {
                x1,
                y1,
                x2,
                y2,
                stroke,
            } => {
                out.push_str(&format!(
                    "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"{}/>\n",
                    num(*x1),
                    num(*y1),
                    num(*x2),
                    num(*y2),
                    stroke_attrs(Some(*stroke))
                ));
            }
            Item::Path { cmds, fill, stroke } => {
                out.push_str(&format!(
                    "<path d=\"{}\"{}{}",
                    path_data(cmds),
                    fill_attrs(*fill),
                    stroke_attrs(*stroke)
                ));
                if stroke.is_some() {
                    out.push_str(" stroke-linejoin=\"round\" stroke-linecap=\"round\"");
                }
                out.push_str("/>\n");
            }
            Item::Circle {
                cx,
                cy,
                r,
                fill,
                stroke,
            } => {
                out.push_str(&format!(
                    "<circle cx=\"{}\" cy=\"{}\" r=\"{}\"{}{}/>\n",
                    num(*cx),
                    num(*cy),
                    num(*r),
                    fill_attrs(*fill),
                    stroke_attrs(*stroke)
                ));
            }
            Item::Text(t) => {
                let anchor = match t.anchor {
                    Anchor::Start => "",
                    Anchor::Middle => " text-anchor=\"middle\"",
                    Anchor::End => " text-anchor=\"end\"",
                };
                out.push_str(&format!(
                    "<text x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"{}\"{anchor}",
                    num(t.x),
                    num(t.y),
                    num(t.size),
                    t.color.css()
                ));
                if t.bold {
                    out.push_str(" font-weight=\"600\"");
                }
                if t.rotate != 0.0 {
                    out.push_str(&format!(
                        " transform=\"rotate({} {} {})\"",
                        num(t.rotate),
                        num(t.x),
                        num(t.y)
                    ));
                }
                out.push('>');
                out.push_str(&xml_escape(&t.text));
                out.push_str("</text>\n");
            }
        }
    }
    out.push_str("</svg>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_compact_and_stable() {
        assert_eq!(num(1.0), "1");
        assert_eq!(num(1.256), "1.26");
        assert_eq!(num(-0.001), "0");
        assert_eq!(num(f32::NAN), "0");
    }

    #[test]
    fn text_is_escaped_and_nothing_executable_is_emitted() {
        let mut scene = Scene::new(10.0, 10.0);
        scene.title = "</title><script>alert(1)</script>".into();
        scene.text(
            1.0,
            1.0,
            "<script>alert(\"x\")</script>",
            10.0,
            Rgb(0, 0, 0),
            Anchor::Start,
        );
        let svg = to_svg(&scene, "c\"onload=\"x");
        assert!(!svg.contains("<script"));
        assert!(!svg.contains("onload=\""));
        assert!(svg.contains("&lt;script&gt;"));
        assert!(!svg.contains("foreignObject"));
    }

    #[test]
    fn arcs_end_where_they_should() {
        let cmds = arc(
            0.0,
            0.0,
            10.0,
            0.0,
            std::f32::consts::PI,
            Cmd::Move(0.0, 0.0),
        );
        assert_eq!(cmds.len(), 3);
        let Cmd::Cubic(.., x, y) = cmds[2] else {
            panic!()
        };
        assert!((x - 0.0).abs() < 1e-3 && (y - 10.0).abs() < 1e-3);
    }
}
